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
    GlobalShippingChokepointPosition, GlobalShippingChokepointShip, GlobalShippingChokepointUpdate,
};
use smol_str::{SmolStr, format_smolstr};
use time::{OffsetDateTime, UtcOffset};

use crate::scene::Piece;

/// The west edge of the map, in degrees east.
pub(super) const MAP_WEST: f32 = 47.802;
/// The east edge of the map, in degrees east.
pub(super) const MAP_EAST: f32 = 61.875;
/// The south edge of the map, in degrees north.
///
/// Half a degree below the screenshot's box: the fitted scale covers the viewport
/// rather than fitting inside it, so a pane wider than the map's own aspect crops
/// the top and bottom instead of showing an ocean-coloured band there.
pub(super) const MAP_SOUTH: f32 = 21.443;
/// The north edge of the map, in degrees north.
pub(super) const MAP_NORTH: f32 = 30.042;

/// The culling grid's columns, west to east.
pub(super) const GRID_COLS: u32 = 64;
/// The culling grid's rows, south to north.
pub(super) const GRID_ROWS: u32 = 32;

/// The build's simplification tolerance for the coarse copy of class 0, in degrees.
///
/// `assets/build_map.py` writes `COARSE_ROAD_SIMPLIFY = 0.008`. **Changing it means
/// re-running the build, updating this constant and re-measuring the budget** — the
/// runtime threshold below is derived from it.
pub(super) const ROAD_COARSE_TOLERANCE_DEGREES: f32 = 0.008;

/// The build's simplification tolerance for the middle copy of class 0, in degrees.
///
/// `assets/build_map.py` writes `MIDDLE_ROAD_SIMPLIFY = 0.004`. It is a second
/// geometry of the same long chains the coarse copy holds, for the scales where the
/// coarse copy would be more than 1.2 px off but the fine copy is still too heavy.
/// **Changing it means re-running the build, updating this constant and re-measuring
/// the budget** — the runtime threshold below is derived from it.
pub(super) const ROAD_MIDDLE_TOLERANCE_DEGREES: f32 = 0.004;

/// The metres one degree of latitude covers: the worst axis of a Douglas–Peucker
/// tolerance.
///
/// A tolerance is isotropic in degrees, but a degree of latitude is 111,320 m while a
/// degree of longitude at [`MAP_SCALE_LATITUDE`] is `111,320 × cos(25.7425°) ≈
/// 100,160 m`. The worst displacement is therefore on the latitude axis, and that is
/// the figure the 1.2 px threshold is derived from. `assets/build_map.py` uses the same
/// 111,320 for its length statistics.
const ROAD_METRES_PER_DEGREE: f32 = 111_320.0;

/// The worst distance the coarse copy may be off where it starts being drawn, in
/// pixels.
///
/// The same 1.2 px budget the coastline tiers use
/// (`LAND_TIER_WORST_CASE_AT_THRESHOLD_PX` in the tests below). A Douglas–Peucker
/// simplification moves a line by at most its tolerance, so the tolerance in metres
/// divided by this budget is the coarsest scale the copy may be drawn at.
pub(super) const ROAD_WORST_CASE_AT_THRESHOLD_PX: f32 = 1.2;

/// The metres per pixel at and above which the coarse copy of class 0 is drawn.
///
/// Derived, not chosen: the build's 0.008° is 890.6 m on the latitude axis, and
/// `890.6 / 1.2 ≈ 742` is the coarsest scale at which that copy is still within the
/// 1.2 px budget. Above it (a fitted view on a wide pane, or any view zoomed further
/// out) the coarse copy is drawn; below it the middle copy takes over. A
/// 2,048 px wide pane is 688 m/px at 1×, so it draws the middle copy.
pub(super) const ROAD_COARSE_METRES_PER_PIXEL: f32 =
    ROAD_COARSE_TOLERANCE_DEGREES * ROAD_METRES_PER_DEGREE / ROAD_WORST_CASE_AT_THRESHOLD_PX;

/// The metres per pixel at and above which the middle copy of class 0 is drawn.
///
/// Derived exactly like the coarse threshold: the build's 0.004° is 445.3 m on the
/// latitude axis, and `445.3 / 1.2 ≈ 371` is the coarsest scale at which that copy is
/// still within the 1.2 px budget. Above it the middle copy is drawn; below it the
/// fine copy takes over. The middle copy holds the same long chains as the coarse
/// one, so the short chains feature-dropped there stay dropped through this band and
/// come back only with the fine copy, where they are over a pixel long.
pub(super) const ROAD_MIDDLE_METRES_PER_PIXEL: f32 =
    ROAD_MIDDLE_TOLERANCE_DEGREES * ROAD_METRES_PER_DEGREE / ROAD_WORST_CASE_AT_THRESHOLD_PX;

/// The zoom from which the primary roads join the fine copy.
///
/// The product's requirement is "the closest zoom draws D+E", and [`MAX_ZOOM`] is the
/// literal closest zoom: the primary layer appears at 4× and only there. In the
/// 2.8–3.99× range the picture is the fine class-0 copy alone — the primary network is
/// a visual-density layer that only the closest view justifies. 3.5× was tried first
/// and rejected: the per-way primary layer added about 13,000 triangles at its switch,
/// while the chain-built primary is 18,688 triangles in the asset and 2,646 in the
/// design pane's 4× frame. A metres-per-pixel threshold is wrong because a tall pane
/// would then draw primary from 2.7×. It is deliberately a zoom, not a scale; the two
/// class-0 copies stay scale-based.
pub(super) const ROAD_PRIMARY_ZOOM: f32 = MAX_ZOOM;

/// How many resolutions of coastline the asset packs, finest first.
pub(super) const LAND_TIERS: usize = 4;

/// The metres per pixel at and above which the coarsest coast (0.006°) is drawn.
///
/// The tiers are chosen by the scale the projection actually draws at, never by the
/// camera's zoom number: a zoom is relative to the pane's own fitted scale, so `1×`
/// is 1.0 km/px on the user's 1,404 × 900 pane and 0.7 km/px on a 2,048 px wide one.
/// The threshold is the measured **1,092.9 m** worst deviation on the local
/// equirectangular plane (not the longitude-scaled 1,043.4 m the first pass printed),
/// divided by the 1.2 px budget: `1,092.9 / 1.2 = 910.8`, rounded up to **911**.
/// The worst pair is not purely north-south — `(50.3324772, 25.5430598) →
/// (50.3241615, 25.5367125)`, Δlon 0.0083° and Δlat 0.0063° — so the latitude-axis
/// bound 1,158.4 m overstates it; using that bound would set 965 and needlessly drop
/// the 1,600 px wide pane's fitted 1× frame (880 m/px) to the 0.003° tier. At 911 the
/// tier measures 1.1996 px at the switch and 1.089 px at the design pane's fitted
/// zoom, inside both budgets.
pub(super) const LAND_TIER_COARSE_METRES_PER_PIXEL: f32 = 911.0;

/// The metres per pixel at and above which the middle coast (0.003°) is drawn.
///
/// Its measured **503.4 m** worst deviation is 1.119 px at this threshold and 0.501 px
/// at the design pane's fitted zoom. The exact derived threshold is `503.4 / 1.2 =
/// 419.5`, so 450 is a round-up that leaves 6.7% of the budget unused; it is kept
/// because it is the design's round number and the 0.002° tier covers the scales below
/// it without a gap.
pub(super) const LAND_TIER_MIDDLE_METRES_PER_PIXEL: f32 = 450.0;

/// The metres per pixel at and above which the 0.002° coast is drawn.
///
/// Derived from the measurement the same way the two coarser thresholds are: the
/// tier's worst deviation is **344.8 m** on the local metric, and `344.8 / 1.2 =
/// 287.3` is the coarsest scale at which it is still inside the 1.2 px budget. 288 is
/// that figure rounded up to a whole metre per pixel, so the tier measures 1.197 px at
/// the switch — just inside the budget rather than just outside it. Below it the
/// 0.001° tier is drawn, which is 100 m on the ground and well under a pixel at every
/// scale this threshold leaves it.
pub(super) const LAND_TIER_FINE_METRES_PER_PIXEL: f32 = 288.0;

/// The coastline tier a projection drawing `metres_per_pixel` metres per pixel draws.
///
/// Index 0 is the 0.001° tier, 1 the 0.002° one, 2 the 0.003° one and 3 the 0.006°
/// one; the asset and [`Scene::land`](crate::scene::Scene::land) store them in the
/// same order. The selection is monotone — zooming in never picks a coarser tier —
/// and a scale that is not a positive number (a pane that is not laid out yet) draws
/// the finest one, which is the safe direction: a frame at an unknown scale has no
/// error budget.
pub(super) fn land_tier(metres_per_pixel: f32) -> usize {
    if metres_per_pixel.is_nan() || metres_per_pixel <= 0.0 {
        0
    } else if metres_per_pixel >= LAND_TIER_COARSE_METRES_PER_PIXEL {
        3
    } else if metres_per_pixel >= LAND_TIER_MIDDLE_METRES_PER_PIXEL {
        2
    } else if metres_per_pixel >= LAND_TIER_FINE_METRES_PER_PIXEL {
        1
    } else {
        0
    }
}

/// The flag bit a road piece sets when the way is a primary road.
///
/// Bit 0 of the build's flags; a motorway or a trunk leaves it clear.
pub(super) const ROAD_PRIMARY: u8 = 1 << 0;

/// The flag bit a road piece sets when it belongs to the middle copy of class 0.
///
/// Bit 1 of the build's flags. The middle copy is a third, independently simplified
/// geometry of the same long chains the coarse copy holds: 0.004° instead of 0.008°,
/// drawn from [`ROAD_MIDDLE_METRES_PER_PIXEL`] up to the coarse threshold. It exists
/// because the fine copy at those scales is the heaviest layer in the frame — on a
/// 2,048 px pane at 1.1× it alone was 19,856 triangles — while the coarse copy is
/// more than 1.2 px off there.
pub(super) const ROAD_MIDDLE: u8 = 1 << 1;

/// The flag bit a road piece sets when it belongs to the coarse copy of class 0.
///
/// Bit 2 of the build's flags. The coarse copy is a second, independently simplified
/// geometry of the motorways and trunks: it exists so the network can be drawn at the
/// fitted zoom, where the fine copy's detail is below a pixel. It holds only the chains
/// at least 0.3 km long; the short ones are feature-dropped there and come back through
/// the fine copy at the scale below.
pub(super) const ROAD_COARSE: u8 = 1 << 2;

/// The flag bit a road piece sets when its whole chain is shorter than the build's
/// feature-dropping threshold.
///
/// Bit 3 of the build's flags. The build measures every merged class-0 chain, leaves
/// the short ones out of the coarse copy entirely, and sets this bit on every piece of
/// one in the fine copy, so a chain is drawn whole or not at all. A frame whose pixels
/// are coarser than [`ROAD_SHORT_CHAIN_METRES_PER_PIXEL`] skips these pieces; a closer
/// one draws them. Dropping a whole chain keeps every drawn road continuous, which
/// dropping a piece of a longer chain would not.
pub(super) const ROAD_SHORT_CHAIN: u8 = 1 << 3;

/// The metres per pixel at and below which the short chains are drawn.
///
/// 628 m/px is just coarser than the design pane's 1.6× (627.5 m/px), so every chain
/// the build flagged is drawn at 1.6× on that pane, while the fitted 1× frame
/// (1,004 m/px) and the 1.5× frame (669 m/px) drop them. Like every other road
/// threshold this is the picture's real scale: a narrow pane reaches it later, and a
/// wide one earlier.
pub(super) const ROAD_SHORT_CHAIN_METRES_PER_PIXEL: f32 = 628.0;

/// How much of the road network one camera scale draws.
///
/// The three copies of class 0 are mutually exclusive — a frame draws one of them and
/// never two — so no road is drawn twice. The primary layer is independent of the copy
/// and joins at the closest zoom; a scale that still draws the coarse copy keeps it
/// out, because the coarse copy is already a fitted-zoom picture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RoadFilter {
    /// The coarse copy of the motorways and trunks: every piece the asset stores.
    CoarseCopy,
    /// The middle copy of the motorways and trunks: every piece the asset stores.
    MiddleCopy,
    /// The middle copy and the primary roads.
    MiddleCopyAndPrimary,
    /// The fine copy of the motorways and trunks: every piece the asset stores.
    FineCopy,
    /// The fine copy and the primary roads.
    FineCopyAndPrimary,
}

/// The filter a projection drawing `metres_per_pixel` metres per pixel at `zoom` uses.
///
/// The three class-0 copies are chosen by the picture's real scale, never by the zoom
/// number: a zoom is relative to the pane's own fitted scale, so the same 2.8× is
/// 359 m/px on the design pane and 246 m/px on a 2,048 px wide one. The primary layer
/// is the one exception, and [`ROAD_PRIMARY_ZOOM`] says why. A scale that is not a
/// positive number (a pane that is not laid out yet) reads as the finest copy, which is
/// the safe direction: a frame at an unknown scale has no error budget.
pub(super) fn road_filter(metres_per_pixel: f32, zoom: f32) -> RoadFilter {
    let primary = zoom >= ROAD_PRIMARY_ZOOM;
    if metres_per_pixel >= ROAD_COARSE_METRES_PER_PIXEL {
        RoadFilter::CoarseCopy
    } else if metres_per_pixel >= ROAD_MIDDLE_METRES_PER_PIXEL {
        if primary {
            RoadFilter::MiddleCopyAndPrimary
        } else {
            RoadFilter::MiddleCopy
        }
    } else if primary {
        RoadFilter::FineCopyAndPrimary
    } else {
        RoadFilter::FineCopy
    }
}

/// The path one road piece is drawn in under `filter`, or `None` when it is dropped.
///
/// Path 0 holds motorways and trunks and path 1 the primary roads. The flags are decoded
/// here and nowhere else, and the filter and the painter's split are the same answer, so
/// a piece cannot be drawn in a path the filter had already dropped. The copy bits are
/// what keep the three geometries apart: a coarse piece is drawn only at the coarse
/// filter, a middle one only at the middle filters and a fine one only below both, which
/// is why none is ever drawn twice.
///
/// The short-chain bit is a second, independent gate: a piece of a chain shorter than
/// the build's threshold is drawn only when a pixel is at most
/// [`ROAD_SHORT_CHAIN_METRES_PER_PIXEL`]. The build sets it on every piece of such a
/// chain, so the gate always drops or keeps a whole road. Only the fine copy holds
/// those pieces at all; the middle and coarse copies leave them out at build time.
pub(super) fn road_path(filter: RoadFilter, piece: &Piece, metres_per_pixel: f32) -> Option<usize> {
    if piece.flags & ROAD_SHORT_CHAIN != 0 && metres_per_pixel > ROAD_SHORT_CHAIN_METRES_PER_PIXEL {
        return None;
    }
    if piece.flags & ROAD_COARSE != 0 {
        return (filter == RoadFilter::CoarseCopy).then_some(0);
    }
    if piece.flags & ROAD_MIDDLE != 0 {
        return matches!(
            filter,
            RoadFilter::MiddleCopy | RoadFilter::MiddleCopyAndPrimary
        )
        .then_some(0);
    }
    if piece.flags & ROAD_PRIMARY != 0 {
        return matches!(
            filter,
            RoadFilter::MiddleCopyAndPrimary | RoadFilter::FineCopyAndPrimary
        )
        .then_some(1);
    }
    if matches!(
        filter,
        RoadFilter::CoarseCopy | RoadFilter::MiddleCopy | RoadFilter::MiddleCopyAndPrimary
    ) {
        return None;
    }
    Some(0)
}

/// The most vessels one picture holds.
///
/// The service picks which vessels those are, by distance; this is only the bound a
/// keyframe is checked against, because a client that kept every vessel it ever saw
/// would accumulate a set the service no longer holds.
pub(super) const MAX_VESSELS: usize = 1_000;

/// The furthest out the camera goes: the fitted scale, which covers the viewport.
pub(super) const MIN_ZOOM: f32 = 1.0;
/// The furthest in the camera goes.
pub(super) const MAX_ZOOM: f32 = 4.0;

/// How far a pointer may be from a vessel and still hit it, in pixels.
pub(super) const HIT_RADIUS_PX: f32 = 8.0;

/// How long a picture may go without a push and still be called live.
///
/// The service refreshes at least once a minute, so five minutes of silence is several
/// refreshes missed: the picture on screen is then a report about the past rather than a
/// live one. The window is generous on purpose — the badge it arms is a warning, and a
/// warning that fires between two ordinary refreshes would be noise.
pub(super) const STALE_AFTER: Duration = Duration::from_secs(300);

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

/// The latitude a Mercator `y` stands for, in degrees.
///
/// The culling grid's rows are latitude bands: the build cut them in degrees, so the
/// row axis is read back through the projection instead of off the Mercator `y`.
/// Over this box the two differ by up to a quarter of a row, which is enough to put a
/// piece in the cell beside the one the asset's own cell table put it in.
pub(super) fn latitude_of(mercator_y: f32) -> f32 {
    (2.0 * mercator_y.exp().atan() - std::f32::consts::FRAC_PI_2).to_degrees()
}

/// The pixels one Mercator unit is drawn at when the map is fitted to the viewport.
///
/// The fit **covers** the viewport: the larger of the two axis ratios, so the map
/// always fills the pane and the axis it is proportionally longer than is cropped.
/// A `contain` fit (the smaller ratio) left an ocean-coloured band above and below on
/// a pane wider than the map's aspect, which is the defect this replaced.
/// The camera's scale is this times [`Camera::zoom`]. A viewport with no area — a
/// pane laid out at zero size, before its first frame — has no fit at all and reads
/// as zero, which the camera treats as the degenerate viewport it is.
pub(super) fn fit_px_per_merc(viewport: [f32; 2]) -> f32 {
    let [width, height] = viewport;
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return 0.0;
    }

    let [west, south, east, north] = mercator_bounds();
    (width / (east - west)).max(height / (north - south))
}

/// The latitude the map's own scale is read at, in degrees.
///
/// The map is a Mercator picture whose rows stretch towards the poles, so "metres per
/// pixel" needs a row to mean anything. This is the map box's own middle — near the
/// strait the picture is about — rather than the camera's: the two differ by up to
/// 7% on this box, and letting a pan move the coastline tier would make the coast
/// pop in and out while dragging.
const MAP_SCALE_LATITUDE: f32 = (MAP_SOUTH + MAP_NORTH) / 2.0;

/// How many metres one pixel covers when a Mercator unit is drawn at `scale` pixels.
///
/// This is the real scale of the picture, and it is what the coastline tier is chosen
/// by — never the zoom number, which is relative to a fit that changes with the
/// pane's size. A Mercator `x` is a radian of longitude, and one radian of longitude is
/// the Earth's radius times the cosine of the latitude — about 5,730 km here, not the
/// 40,030 km a full circumference would give. A degenerate scale (a pane with no size)
/// reads as zero, which is "the finest coast there is": a frame that cannot draw has
/// nothing to be coarse about.
pub(super) fn metres_per_pixel(scale: f32) -> f32 {
    if !scale.is_finite() || scale <= 0.0 {
        return 0.0;
    }
    // The Earth's mean radius, in metres (IUGG).
    const EARTH_RADIUS_M: f32 = 6_371_008.8;
    EARTH_RADIUS_M * MAP_SCALE_LATITUDE.to_radians().cos() / scale
}

/// The grid cells a viewport touches, as inclusive column and row indices.
///
/// A span rather than a `RangeInclusive` pair because the frame hands it to the land
/// layer and, once the line layers are drawn, to those too: `RangeInclusive` is not
/// `Copy`, so it could not be passed three times. A span whose start is past its end
/// covers nothing — what a viewport entirely off the map reads as.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CellSpan {
    /// The first column the viewport touches, west to east.
    pub col_start: u32,
    /// The last column the viewport touches.
    pub col_end: u32,
    /// The first row the viewport touches, south to north.
    pub row_start: u32,
    /// The last row the viewport touches.
    pub row_end: u32,
}

impl CellSpan {
    /// The span that covers no cell at all.
    const EMPTY: Self = Self {
        col_start: 1,
        col_end: 0,
        row_start: 1,
        row_end: 0,
    };

    /// Whether the span covers no cell.
    pub(super) fn is_empty(self) -> bool {
        self.col_start > self.col_end || self.row_start > self.row_end
    }

    /// The grid index of every cell the span covers, in row-major order.
    ///
    /// The index is `row * GRID_COLS + col`, which is what the asset's cell tables
    /// and the land layer's buckets are keyed by.
    pub(super) fn indices(self) -> impl Iterator<Item = u32> {
        (self.row_start..=self.row_end).flat_map(move |row| {
            (self.col_start..=self.col_end).map(move |col| row * GRID_COLS + col)
        })
    }

    /// The span grown by one cell on every side, clamped to the grid.
    ///
    /// The land is bucketed by triangle centroid, so a triangle whose centroid falls
    /// just outside the viewport can still reach into it. The line layers are cut per
    /// cell by the build and need no margin. An empty span stays empty: growing it
    /// would invent a cell a viewport off the map never touched.
    pub(super) fn grown(self) -> Self {
        if self.is_empty() {
            return self;
        }
        Self {
            col_start: self.col_start.saturating_sub(1),
            col_end: self.col_end.saturating_add(1).min(GRID_COLS - 1),
            row_start: self.row_start.saturating_sub(1),
            row_end: self.row_end.saturating_add(1).min(GRID_ROWS - 1),
        }
    }
}

/// The cells a Mercator rectangle touches, clamped to the grid.
///
/// `visible` is `[west, south, east, north]` in Mercator radians. A rectangle that
/// does not overlap the map at all reads as an empty span, whose start is past its
/// end, so every layer it is handed to draws nothing.
pub(super) fn visible_cells(visible: [f32; 4]) -> CellSpan {
    let [west, _, east, _] = mercator_bounds();
    if !visible.iter().all(|edge| edge.is_finite()) {
        return CellSpan::EMPTY;
    }
    let Some((col_start, col_end)) = cell_range(visible[0], visible[2], west, east, GRID_COLS)
    else {
        return CellSpan::EMPTY;
    };
    // The rows are latitude bands, not Mercator bands: the build cut the asset's cell
    // table in degrees, so the viewport's Mercator edges are converted back first.
    let Some((row_start, row_end)) = cell_range(
        latitude_of(visible[1]),
        latitude_of(visible[3]),
        MAP_SOUTH,
        MAP_NORTH,
        GRID_ROWS,
    ) else {
        return CellSpan::EMPTY;
    };
    CellSpan {
        col_start,
        col_end,
        row_start,
        row_end,
    }
}

/// The inclusive cells one axis of a rectangle touches, or `None` when it misses the map.
///
/// The low edge selects the cell it falls in and the high edge the last cell it
/// reaches into, so a rectangle ending exactly on a grid line does not claim the cell
/// past it. Both are clamped: a viewport wider than the map touches every cell, not
/// cells that do not exist, and a degenerate one on the far edge still touches the
/// edge cell.
fn cell_range(low: f32, high: f32, map_low: f32, map_high: f32, cells: u32) -> Option<(u32, u32)> {
    if !low.is_finite() || !high.is_finite() || high < low {
        return None;
    }
    if low > map_high || high < map_low {
        return None;
    }

    let last = cells as f32 - 1.0;
    let per_cell = cells as f32 / (map_high - map_low);
    let first = ((low - map_low) * per_cell).floor().clamp(0.0, last);
    let reached = (((high - map_low) * per_cell).ceil() - 1.0).clamp(0.0, last);
    let reached = reached.max(first);
    Some((first as u32, reached as u32))
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
    /// The fit covers the viewport (see [`fit_px_per_merc`]), so at every zoom the
    /// map is at least as large as the pane on both axes: the centre is clamped to
    /// keep the map's edge outside the viewport, and a drag can never expose the
    /// canvas colour behind the sea. An axis whose half-extent does not fit (only
    /// reachable when the pane's aspect matches the map's, so both edges coincide)
    /// centres the map instead of clamping a degenerate interval. A viewport with no
    /// fit at all centres the whole map. Zoom is clamped here because it is the one
    /// field a scroll wheel writes.
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

/// One axis of [`Camera::clamped`]: clamped so the map covers the viewport.
///
/// `half` is the visible half-extent in Mercator units. A cover fit keeps
/// `low + half <= high - half` on both axes; the equality case (a pane whose aspect
/// exactly matches the map's, possibly off by one rounding step) has no interval to
/// clamp into, so it reads as the map's centre rather than as an inverted `f32::clamp`.
fn clamp_center(center: f32, half: f32, low: f32, high: f32) -> f32 {
    let (low_edge, high_edge) = (low + half, high - half);
    if low_edge >= high_edge {
        (low + high) / 2.0
    } else {
        center.clamp(low_edge, high_edge)
    }
}

/// Where the map's Mercator points land in the canvas.
#[derive(Clone, Copy, Debug)]
pub(super) struct Projection {
    /// The Mercator point drawn at the centre of the canvas.
    center: [f32; 2],
    /// The pixels one Mercator unit is drawn at.
    scale: f32,
    /// The canvas' size.
    pub viewport: [f32; 2],
    /// The camera's zoom, which is what the road detail and the marker size read.
    pub zoom: f32,
}

impl Projection {
    /// The projection a camera lays over a canvas of `viewport` pixels.
    pub(super) fn new(camera: Camera, viewport: [f32; 2]) -> Self {
        Self {
            center: camera.center,
            scale: fit_px_per_merc(viewport) * camera.zoom,
            viewport,
            zoom: camera.zoom,
        }
    }

    /// Whether the projection can draw anything at all.
    ///
    /// A pane laid out at zero size has no fit, and a marker drawn through a zero scale is
    /// a degenerate triangle rather than a vessel.
    pub(super) fn drawable(self) -> bool {
        self.scale.is_finite()
            && self.scale > 0.0
            && self.viewport[0] > 0.0
            && self.viewport[1] > 0.0
    }

    /// The canvas point one Mercator point is drawn at.
    ///
    /// A Mercator `y` grows north and a canvas `y` grows down, so the two run in opposite
    /// directions: the centre is subtracted the other way round for `y`, which is what puts
    /// the Arabian side under the Iranian one rather than over it.
    pub(super) fn screen(self, mercator: [f32; 2]) -> [f32; 2] {
        [
            (mercator[0] - self.center[0]) * self.scale + self.viewport[0] / 2.0,
            (self.center[1] - mercator[1]) * self.scale + self.viewport[1] / 2.0,
        ]
    }

    /// The Mercator point drawn at one canvas point, which is what a zoom keeps fixed.
    pub(super) fn mercator(self, canvas: [f32; 2]) -> [f32; 2] {
        if !self.drawable() {
            return self.center;
        }
        [
            self.center[0] + (canvas[0] - self.viewport[0] / 2.0) / self.scale,
            self.center[1] - (canvas[1] - self.viewport[1] / 2.0) / self.scale,
        ]
    }

    /// The Mercator rectangle the canvas shows, as `[west, south, east, north]`.
    ///
    /// This is what the culling grid is read from: a layer indexes only the cells this
    /// rectangle touches. Only meaningful for a drawable projection — a pane with no
    /// size has no scale to divide by, and every painter checks [`Self::drawable`]
    /// before it asks.
    pub(super) fn visible_mercator(self) -> [f32; 4] {
        let half = [
            self.viewport[0] / (2.0 * self.scale),
            self.viewport[1] / (2.0 * self.scale),
        ];
        [
            self.center[0] - half[0],
            self.center[1] - half[1],
            self.center[0] + half[0],
            self.center[1] + half[1],
        ]
    }

    /// The metres one pixel covers, at the latitude the map is read at.
    ///
    /// This is the real scale of the picture, and it is what the coastline tier is
    /// chosen by: it depends on the pane's size and the camera's zoom, but not on
    /// where the camera points. See [`metres_per_pixel`].
    pub(super) fn metres_per_pixel(self) -> f32 {
        metres_per_pixel(self.scale)
    }

    /// Whether a canvas point is inside the canvas grown by `margin` on every side.
    pub(super) fn near_screen(self, canvas: [f32; 2], margin: f32) -> bool {
        canvas[0] >= -margin
            && canvas[0] <= self.viewport[0] + margin
            && canvas[1] >= -margin
            && canvas[1] <= self.viewport[1] + margin
    }

    /// Whether the box one screen-space segment covers reaches the canvas.
    ///
    /// The endpoints of a segment are not enough: a lane's segment can leave the canvas on
    /// both ends while crossing it.
    pub(super) fn segment_near(self, start: [f32; 2], end: [f32; 2], margin: f32) -> bool {
        let left = start[0].min(end[0]);
        let right = start[0].max(end[0]);
        let top = start[1].min(end[1]);
        let bottom = start[1].max(end[1]);

        right >= -margin
            && left <= self.viewport[0] + margin
            && bottom >= -margin
            && top <= self.viewport[1] + margin
    }
}

/// The label layers, in the order the map places them when two anchors collide.
///
/// Countries and seas are large areas the reader orients by; a city is a point inside
/// them. That order is also the priority: a city name never displaces a country or a sea
/// name, and every list keeps its authored order, so the same camera always places the
/// same labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LabelKind {
    /// A country name, drawn upper case over the land.
    Country,
    /// A body of water's name, drawn italic over the sea.
    Water,
    /// A city name, drawn at its anchor.
    City,
}

/// One label as the frame places it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlacedLabel {
    /// Which layer the label belongs to.
    pub kind: LabelKind,
    /// The catalogue key the name is read from.
    pub key: &'static str,
    /// The canvas point the text is centred on.
    pub at: [f32; 2],
}

/// The zoom the cities are named from: the fitted zoom itself.
///
/// A city is a point, so its name is only worth drawing once the map is at least as
/// detailed as the fitted view; lowering this from 1.6 to the fit is what shows the
/// strait's cities at 1×. The declutter rule below keeps them from piling up there.
pub(super) const CITY_LABEL_ZOOM: f32 = MIN_ZOOM;

/// How close two placed anchors may sit before the later label is dropped, in pixels.
///
/// The cities around the strait are the crowded case: at the fitted zoom their anchors are
/// a few dozen pixels apart while their centred names are up to a hundred pixels wide, so
/// drawing every one of them would print a single unreadable pile. The rule is
/// anchor-to-anchor rather than text-to-text because the names come from the catalogue and
/// change with the language: a fixed screen distance is deterministic and
/// language-independent, and at this distance no two names this map draws overlap (the
/// test below measures the boxes on the 1× view).
pub(super) const LABEL_MIN_SEPARATION_PX: f32 = 72.0;

/// How far inside the canvas a label's anchor has to sit for the name to fit, in pixels.
///
/// A label is centred on its anchor, so half of it hangs to each side: an anchor nearer the
/// canvas edge than half the name's width would have that half cut off by the pane. A city's
/// or a sea's name is short enough that half the longest of them is this margin; a country's
/// is the long one, and it has its own ([`COUNTRY_LABEL_EDGE_MARGIN_PX`]). A label that
/// cannot fit is not drawn — a name cut in half at the pane's edge reads as a defect, while
/// a name that appears when the map is panned away from the edge does not.
pub(super) const LABEL_EDGE_MARGIN_PX: f32 = 80.0;

/// The margin a country's name needs: half of the widest of them, `UNITED ARAB EMIRATES`.
///
/// 141 px at the 12 px size, so about 247 px at the country's 21 px step (a little more in
/// bold italic); half of that, rounded up. Keeping it out of [`LABEL_EDGE_MARGIN_PX`] is
/// what lets the country grow without costing the fitted view any of its cities.
const COUNTRY_LABEL_EDGE_MARGIN_PX: f32 = 130.0;

/// The labels one frame draws, in priority order.
///
/// A label whose anchor is too close to the canvas edge for its name to fit (see
/// [`LABEL_EDGE_MARGIN_PX`]) is not built at all, and one whose anchor is closer than
/// [`LABEL_MIN_SEPARATION_PX`] to a label already placed is skipped: the first of a crowded
/// pair is the higher-priority one, because the layers are visited in that order.
pub(super) fn place_labels(projection: Projection) -> Vec<PlacedLabel> {
    let mut placed: Vec<PlacedLabel> =
        Vec::with_capacity(COUNTRY_LABELS.len() + WATER_LABELS.len() + CITY_LABELS.len());
    for (kind, labels) in [
        (LabelKind::Country, &COUNTRY_LABELS[..]),
        (LabelKind::Water, &WATER_LABELS[..]),
        (LabelKind::City, &CITY_LABELS[..]),
    ] {
        if kind == LabelKind::City && projection.zoom < CITY_LABEL_ZOOM {
            continue;
        }
        for label in labels {
            let at = projection.screen(project(label.at[0], label.at[1]));
            let [width, height] = projection.viewport;
            let margin = if kind == LabelKind::Country {
                COUNTRY_LABEL_EDGE_MARGIN_PX
            } else {
                LABEL_EDGE_MARGIN_PX
            };
            if at[0] < margin || at[0] > width - margin || at[1] < margin || at[1] > height - margin
            {
                continue;
            }
            let crowded = placed.iter().any(|other| {
                let [dx, dy] = [other.at[0] - at[0], other.at[1] - at[1]];
                dx * dx + dy * dy < LABEL_MIN_SEPARATION_PX * LABEL_MIN_SEPARATION_PX
            });
            if !crowded {
                placed.push(PlacedLabel {
                    kind,
                    key: label.key,
                    at,
                });
            }
        }
    }
    placed
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
    /// The identity its position and its ship record are joined on.
    pub mmsi: u32,
    /// The bucket its ship type falls in, or [`ShipClass::Other`] while the ship
    /// record has not arrived.
    pub class: ShipClass,
    /// Its last position, in Mercator coordinates.
    pub position: [f32; 2],
    /// Course over ground in 0.1°, as the wire carries it; the marker turns by it and
    /// falls back to the heading when it is at its sentinel.
    pub cog: u32,
    /// True heading in degrees, as the wire carries it.
    pub heading: u32,
    /// Whether the ship record has arrived. A vessel without one is still painted —
    /// as an unknown — because its position is enough to draw a dot.
    pub has_ship: bool,
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
/// because the ship record has not arrived. The order of the fields is the order the
/// design lists the items in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct VesselDetails {
    /// 1. The name, or the MMSI when the report carries an empty one; `None` before
    ///    the ship record arrives.
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
        ship: Option<&GlobalShippingChokepointShip>,
        position: &GlobalShippingChokepointPosition,
        zone: UtcOffset,
    ) -> Self {
        Self {
            name: ship.map(|ship| {
                if ship.name.is_empty() {
                    format_smolstr!("{mmsi}")
                } else {
                    ship.name.clone()
                }
            }),
            mmsi: format_smolstr!("{mmsi}"),
            imo: ship.and_then(|ship| (ship.imo != 0).then(|| format_smolstr!("{}", ship.imo))),
            call_sign: ship
                .and_then(|ship| (!ship.call_sign.is_empty()).then(|| ship.call_sign.clone())),
            ship_class: ship
                .and_then(|ship| (ship.ship_type != 0).then(|| class_of(ship.ship_type))),
            flag: ship.and_then(|ship| flag_for_mid(ship.flag_mid)),
            destination: ship
                .and_then(|ship| (!ship.destination.is_empty()).then(|| ship.destination.clone())),
            eta: ship.and_then(|ship| {
                let carries_eta = ship.eta_month != 0
                    || ship.eta_day != 0
                    || ship.eta_hour != 0
                    || ship.eta_minute != 0;
                carries_eta.then(|| {
                    format_smolstr!(
                        "{:02}-{:02} {:02}:{:02}",
                        ship.eta_month,
                        ship.eta_day,
                        ship.eta_hour,
                        ship.eta_minute
                    )
                })
            }),
            draught: ship.and_then(|ship| {
                (ship.draught != 0)
                    .then(|| format_smolstr!("{}.{} m", ship.draught / 10, ship.draught % 10))
            }),
            dimensions: ship.and_then(|ship| {
                let length = ship.dim_to_bow.saturating_add(ship.dim_to_stern);
                let breadth = ship.dim_to_port.saturating_add(ship.dim_to_starboard);
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
    ships: AHashMap<u32, GlobalShippingChokepointShip>,
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
            ships: AHashMap::with_capacity(MAX_VESSELS),
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
            self.ships.clear();
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

            self.ships
                .extend(update.ships.iter().map(|entry| (entry.mmsi, entry.clone())));
            self.positions
                .extend(update.positions.iter().map(|entry| (entry.mmsi, *entry)));
            self.applied = update.sequence;
            self.gap = false;
            self.has_state = true;
            self.received_at = Instant::now();
            return;
        }

        if update.epoch == self.epoch && update.base == self.applied {
            for ship in &update.ships {
                self.ships.insert(ship.mmsi, ship.clone());
            }
            for position in &update.positions {
                self.positions.insert(position.mmsi, *position);
            }
            for mmsi in &update.removed {
                self.ships.remove(mmsi);
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
        self.ships.clear();
        self.positions.clear();
        self.epoch = 0;
        self.applied = 0;
        self.gap = false;
        self.has_state = false;
        self.received_at = Instant::now();
    }

    /// Every vessel that has a position, joined with its ship record by MMSI.
    ///
    /// A vessel whose ship record has not arrived is still painted — as an unknown
    /// dot — while one that has only a ship record is not painted at all, because
    /// there is nowhere to paint it. The order is the store's, not a ranking.
    ///
    /// Building the list projects every position, so a painter keeps it between
    /// pushes and rebuilds it when one arrives.
    pub fn vessels(&self) -> Vec<Vessel> {
        let mut vessels = Vec::with_capacity(self.positions.len());
        for position in self.positions.values() {
            let ship = self.ships.get(&position.mmsi);
            vessels.push(Vessel {
                mmsi: position.mmsi,
                class: ship.map_or(ShipClass::Other, |ship| class_of(ship.ship_type)),
                position: project(
                    position.longitude as f32 / 1_000_000.0,
                    position.latitude as f32 / 1_000_000.0,
                ),
                cog: position.cog,
                heading: position.heading,
                has_ship: ship.is_some(),
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
            self.ships.get(&mmsi),
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

    /// One ship record, with every value a report can carry.
    fn ships(mmsi: u32) -> GlobalShippingChokepointShip {
        GlobalShippingChokepointShip {
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

    /// A ship record that carries nothing, as a report with every sentinel set.
    fn empty_ship(mmsi: u32) -> GlobalShippingChokepointShip {
        GlobalShippingChokepointShip {
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
            ships: Vec::new(),
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
            ships: Vec::new(),
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
    fn a_camera_should_keep_the_map_over_the_viewport_on_every_edge() {
        // The fit covers the pane, so a drag can never pull an edge of the map inside
        // it and show the canvas colour behind the sea. Checked on a wide, a portrait
        // and a square-ish pane: the cover fit picks a different axis on each.
        let bounds = mercator_bounds();
        for viewport in [[800.0, 600.0], [400.0, 1_200.0], [1_600.0, 866.0]] {
            let zoom = 2.0;
            let scale = fit_px_per_merc(viewport) * zoom;
            let half = [viewport[0] / (2.0 * scale), viewport[1] / (2.0 * scale)];
            assert!(
                half[0] * 2.0 <= bounds[2] - bounds[0] + 1e-6
                    && half[1] * 2.0 <= bounds[3] - bounds[1] + 1e-6,
                "a fitted {viewport:?} pane is never larger than the map on either axis",
            );

            for center in [
                [bounds[0] - 5.0, bounds[1] - 5.0],
                [bounds[2] + 5.0, bounds[3] + 5.0],
                [bounds[0] + 0.1, bounds[3] - 0.1],
                [bounds[2] - 0.1, bounds[1] + 0.1],
            ] {
                let camera = Camera { center, zoom }.clamped(viewport);
                let visible_west = camera.center[0] - half[0];
                let visible_east = camera.center[0] + half[0];
                let visible_south = camera.center[1] - half[1];
                let visible_north = camera.center[1] + half[1];

                assert!(
                    visible_west >= bounds[0] - 1e-6 && visible_east <= bounds[2] + 1e-6,
                    "{viewport:?} from {center:?} shows {visible_west}..{visible_east} \
                     horizontally, outside {}..{}",
                    bounds[0],
                    bounds[2],
                );
                assert!(
                    visible_south >= bounds[1] - 1e-6 && visible_north <= bounds[3] + 1e-6,
                    "{viewport:?} from {center:?} shows {visible_south}..{visible_north} \
                     vertically, outside {}..{}",
                    bounds[1],
                    bounds[3],
                );
            }

            // And the extreme drag lands exactly on the map's edge, not past it.
            let dragged = Camera {
                center: [bounds[0] - 5.0, bounds[1] - 5.0],
                zoom,
            }
            .clamped(viewport);
            assert_close(dragged.center[0], bounds[0] + half[0], "the west edge");
            assert_close(dragged.center[1], bounds[1] + half[1], "the south edge");
        }
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
        assert_eq!(
            too_far_out.zoom, MIN_ZOOM,
            "nor closer than the fitted view that covers the pane"
        );
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
        frame.ships = vec![ships(1), ships(2)];
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
        assert!(!store.ships.contains_key(&1));
        assert!(!store.positions.contains_key(&1));
    }

    #[test]
    fn a_delta_on_the_applied_base_should_be_applied() {
        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 10, vec![position(1)]));

        let mut update = delta(7, 10, 11, vec![position(2)]);
        update.ships = vec![ships(2)];
        store.apply(&update);

        assert!(store.has_state());
        assert!(!store.is_stale());
        assert_eq!(
            store.vessel_count(),
            2,
            "the delta is laid over the keyframe"
        );
        assert!(store.ships.contains_key(&2));
    }

    #[test]
    fn a_removal_should_empty_both_tables() {
        let mut store = VesselStore::new();
        let mut frame = keyframe(7, 1, vec![position(1), position(2)]);
        frame.ships = vec![ships(1), ships(2)];
        store.apply(&frame);

        let mut update = delta(7, 1, 2, Vec::new());
        update.removed = vec![1];
        store.apply(&update);

        assert_eq!(store.vessel_count(), 1);
        assert!(!store.ships.contains_key(&1), "the ship record goes too");
        assert!(store.ships.contains_key(&2));
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
        seeded.ships = vec![ships(1)];
        store.apply(&seeded);
        assert_eq!(
            store.vessel_count(),
            1,
            "there is a picture for the no-state keyframe to clear",
        );

        let mut frame = keyframe(8, 11, vec![position(2)]);
        frame.ships = vec![ships(2)];
        frame.timestamp = 0;
        store.apply(&frame);

        assert!(!store.has_state(), "no state yet is not an empty sea");
        assert!(store.is_stale(), "and it is not a live picture either");
        assert_eq!(store.vessel_count(), 0, "the stale picture was cleared");
        assert!(
            store.ships.is_empty(),
            "the ship record was cleared with it",
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

        // Kept a second beyond the window: at exactly the window the strict `>` would race
        // the clock's resolution rather than test the predicate.
        store.received_at = Instant::now() - (STALE_AFTER + Duration::from_secs(1));
        assert!(
            store.is_stale(),
            "no push for the whole window is a broken link, heartbeat included",
        );
        assert!(
            !store.has_gap(),
            "a silent link is silence rather than a picture that was not continued",
        );

        let mut store = VesselStore::new();
        store.apply(&keyframe(7, 1, vec![position(1)]));
        assert!(!store.is_stale(), "a push just arrived");
        assert!(!store.has_gap());
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
        frame.ships = vec![ships(1), ships(2)];
        store.apply(&frame);

        assert_eq!(
            store.vessel_count(),
            1,
            "a ship record without a position has nowhere to be painted",
        );
        let vessels = store.vessels();
        let vessel = vessels
            .iter()
            .find(|vessel| vessel.mmsi == 1)
            .expect("the vessel with a position is painted");
        assert!(vessel.has_ship);
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
            .expect("a position without a ship record is still a vessel");
        assert!(!vessel.has_ship);
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
        frame.ships = vec![ships(42)];
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
        frame.ships = vec![empty_ship(7)];
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
        let mut reported = ships(42);
        reported.dim_to_bow = u32::MAX;
        reported.dim_to_stern = 1;
        reported.dim_to_port = u32::MAX;
        reported.dim_to_starboard = 2;
        frame.ships = vec![reported];
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
    fn a_vessel_without_a_ship_record_should_read_only_what_its_position_carries() {
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
    fn the_fitted_zoom_names_the_cities_without_printing_them_over_each_other() {
        // The pane the labels were reported on: the user's 1.56:1 viewport at the fitted
        // zoom, where the strait's seven cities sit a few dozen pixels apart.
        let viewport = [1_404.0, 900.0];
        let projection = Projection::new(Camera::default(), viewport);
        let distance = |left: [f32; 2], right: [f32; 2]| {
            ((left[0] - right[0]).powi(2) + (left[1] - right[1]).powi(2)).sqrt()
        };
        let placed = place_labels(projection);
        let keys: Vec<&str> = placed.iter().map(|label| label.key).collect();
        assert_eq!(
            placed,
            place_labels(projection),
            "the order is deterministic"
        );

        // Every label that is in its zoom band and has room for its name but not placed was
        // skipped by a *higher-priority* label within the minimum distance. Countries and
        // seas are never dropped for crowding: nothing on this map sits that close.
        //
        // The labels nearer the canvas edge than [`LABEL_EDGE_MARGIN_PX`] are skipped too,
        // and are the one exception: their centred name would be cut by the pane.
        let [width, height] = viewport;
        let fits = |at: [f32; 2]| {
            at[0] >= LABEL_EDGE_MARGIN_PX
                && at[0] <= width - LABEL_EDGE_MARGIN_PX
                && at[1] >= LABEL_EDGE_MARGIN_PX
                && at[1] <= height - LABEL_EDGE_MARGIN_PX
        };
        let candidates: Vec<(LabelKind, &'static str, [f32; 2])> = [
            (LabelKind::Country, &COUNTRY_LABELS[..]),
            (LabelKind::Water, &WATER_LABELS[..]),
            (LabelKind::City, &CITY_LABELS[..]),
        ]
        .into_iter()
        .flat_map(|(kind, labels)| {
            labels.iter().map(move |label| {
                (
                    kind,
                    label.key,
                    projection.screen(project(label.at[0], label.at[1])),
                )
            })
        })
        .collect();
        for (index, (kind, key, at)) in candidates.iter().enumerate() {
            if !fits(*at) || (*kind == LabelKind::City && projection.zoom < CITY_LABEL_ZOOM) {
                continue;
            }
            if keys.contains(key) {
                continue;
            }
            let crowded = candidates[..index].iter().any(|(_, earlier, earlier_at)| {
                keys.contains(earlier) && distance(*earlier_at, *at) < LABEL_MIN_SEPARATION_PX
            });
            assert!(
                crowded,
                "{key} was skipped without a higher-priority label near its anchor",
            );
        }

        // A conservative box around each anchor: wider than twice the widest city name
        // at the small text size, and one line tall. No two placed labels may overlap it.
        const HALF_WIDTH: f32 = 90.0;
        const HALF_HEIGHT: f32 = 10.0;
        for (index, label) in placed.iter().enumerate() {
            for other in &placed[..index] {
                assert!(
                    (other.at[0] - label.at[0]).abs() >= 2.0 * HALF_WIDTH
                        || (other.at[1] - label.at[1]).abs() >= 2.0 * HALF_HEIGHT,
                    "{} and {} would print over each other",
                    other.key,
                    label.key,
                );
            }
        }

        // The two names the crowded strait drops, and the larger zoom that restores them:
        // Qeshm sits inside Bandar Abbas's name, Ras al Khaimah inside Khasab's.
        let bandar_abbas = "widget.global_shipping_chokepoint.label.city.bandar_abbas";
        let qeshm = "widget.global_shipping_chokepoint.label.city.qeshm";
        let khasab = "widget.global_shipping_chokepoint.label.city.khasab";
        let ras_al_khaimah = "widget.global_shipping_chokepoint.label.city.ras_al_khaimah";
        assert!(keys.contains(&bandar_abbas) && keys.contains(&khasab));
        assert!(!keys.contains(&qeshm), "Qeshm is inside Bandar Abbas");
        assert!(
            !keys.contains(&ras_al_khaimah),
            "Ras al Khaimah is inside Khasab"
        );

        let zoomed = place_labels(Projection::new(
            Camera {
                center: map_centre(),
                zoom: 2.0,
            },
            viewport,
        ));
        let zoomed_keys: Vec<&str> = zoomed.iter().map(|label| label.key).collect();
        assert!(
            zoomed_keys.contains(&qeshm) && zoomed_keys.contains(&ras_al_khaimah),
            "twice the fitted zoom separates the crowded cities",
        );
    }

    #[test]
    fn a_narrow_pane_drops_the_labels_it_cannot_fit_rather_than_cutting_them() {
        // A 320 px column is the narrowest pane a widget is laid out in. A label is
        // centred on its anchor, so one whose anchor is nearer the edge than half its name
        // is dropped instead of printed in halves — Qatar's and the Gulf of Oman's are the
        // two that would be cut at this width, and they are the two that go.
        let viewport = [320.0, 400.0];
        let placed = place_labels(Projection::new(Camera::default(), viewport));
        assert!(!placed.is_empty(), "the pane still names what it can");
        for label in &placed {
            assert!(
                label.at[0] >= LABEL_EDGE_MARGIN_PX
                    && label.at[0] <= viewport[0] - LABEL_EDGE_MARGIN_PX
                    && label.at[1] >= LABEL_EDGE_MARGIN_PX
                    && label.at[1] <= viewport[1] - LABEL_EDGE_MARGIN_PX,
                "{} sits {} px from the edge of a {viewport:?} pane, where half its name \
                 would be cut",
                label.key,
                label.at[0].min(viewport[0] - label.at[0]),
            );
        }
        let keys: Vec<&str> = placed.iter().map(|label| label.key).collect();
        assert!(!keys.contains(&"widget.global_shipping_chokepoint.label.country.qatar"));
        assert!(!keys.contains(&"widget.global_shipping_chokepoint.label.water.gulf_of_oman"));

        // The same anchors on the user's pane are all far enough inside it, so the rule
        // costs the fitted view nothing.
        let wide = place_labels(Projection::new(Camera::default(), [1_404.0, 900.0]));
        assert_eq!(wide.len(), 17, "ten cities, five countries and two seas");
    }

    #[test]
    fn a_marker_should_face_its_course_and_fall_back_to_its_heading() {
        let mut vessel = Vessel {
            mmsi: 1,
            class: ShipClass::Other,
            position: [0.0, 0.0],
            cog: 900,
            heading: 347,
            has_ship: true,
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

    /// The rectangle a camera at `zoom` shows through a `viewport`, in Mercator radians.
    fn visible_rect(viewport: [f32; 2], zoom: f32) -> [f32; 4] {
        let scale = fit_px_per_merc(viewport) * zoom;
        let centre = map_centre();
        let half = [viewport[0] / (2.0 * scale), viewport[1] / (2.0 * scale)];
        [
            centre[0] - half[0],
            centre[1] - half[1],
            centre[0] + half[0],
            centre[1] + half[1],
        ]
    }

    /// The whole grid, which is what a span that covers every cell reads as.
    fn whole_grid() -> CellSpan {
        CellSpan {
            col_start: 0,
            col_end: GRID_COLS - 1,
            row_start: 0,
            row_end: GRID_ROWS - 1,
        }
    }

    #[test]
    fn the_fitted_camera_covers_the_viewport_and_crops_one_axis() {
        // The fit covers rather than contains: at 1× one axis shows exactly the map's
        // span and the other is cropped, so no pane ever sees a band of ocean colour
        // where the map ended. Which axis depends on the pane's aspect against the
        // map's (about 1.48 here).
        let [west, south, east, north] = mercator_bounds();
        let wide = visible_rect([1_600.0, 866.0], MIN_ZOOM);
        assert_close(wide[0], west, "a wide pane shows the west edge exactly");
        assert_close(wide[2], east, "and the east edge exactly");
        assert!(
            wide[1] > south && wide[3] < north,
            "a wide pane crops the rows: {wide:?} inside {south}..{north}",
        );

        let tall = visible_rect([866.0, 1_600.0], MIN_ZOOM);
        assert_close(tall[1], south, "a tall pane shows the south edge exactly");
        assert_close(tall[3], north, "and the north edge exactly");
        assert!(
            tall[0] > west && tall[2] < east,
            "a tall pane crops the columns: {tall:?} inside {west}..{east}",
        );

        // A viewport wider than the map on both axes does not invent cells either.
        let margin = (east - west) / 4.0;
        assert_eq!(
            visible_cells([west - margin, south - margin, east + margin, north + margin,]),
            whole_grid(),
        );
    }

    #[test]
    fn the_visible_cells_are_roughly_a_thirteenth_to_a_sixteenth_at_four_times() {
        // The design measures the culling win on a 1,600×866 pane at four times the
        // fitted zoom. The map's Mercator aspect is about 1.47, so the viewport is wider
        // than the map and the cover fit reads its scale from the width; the height is
        // what gets cropped at 1×, and at 4× the visible grid is about 16 columns × 7 rows.
        let span = visible_cells(visible_rect([1_600.0, 866.0], MAX_ZOOM));
        let columns = span.col_end - span.col_start + 1;
        let rows = span.row_end - span.row_start + 1;
        let visible = f64::from(columns) * f64::from(rows);
        let ratio = f64::from(GRID_COLS * GRID_ROWS) / visible;

        assert!(
            (12.0..=20.0).contains(&ratio),
            "four times the fitted zoom should show about 1/13 of the grid, not 1/{ratio:.1} \
             ({columns} columns × {rows} rows)",
        );
    }

    #[test]
    fn a_viewport_outside_the_map_box_covers_no_cell() {
        let [west, south, east, north] = mercator_bounds();
        for outside in [
            [west - 1.0, south, west - 0.9, north],
            [east + 0.1, south, east + 1.0, north],
            [west, south - 1.0, east, south - 0.9],
            [west, north + 0.1, east, north + 1.0],
        ] {
            let span = visible_cells(outside);
            assert!(span.is_empty(), "{outside:?} is off the map");
            assert_eq!(span.indices().count(), 0, "an empty span draws nothing");
        }

        assert!(
            visible_cells([east, south, west, north]).is_empty(),
            "west past east"
        );
        assert!(visible_cells([f32::NAN, south, east, north]).is_empty());
    }

    #[test]
    fn a_degenerate_viewport_covers_one_cell_and_a_far_edge_still_counts() {
        let [west, south, east, north] = mercator_bounds();
        let centre = map_centre();

        // No area at all: one cell, and its start equals its end.
        let point = visible_cells([centre[0], centre[1], centre[0], centre[1]]);
        assert_eq!(point.col_start, point.col_end);
        assert_eq!(point.row_start, point.row_end);
        assert_eq!(
            point.indices().count(),
            1,
            "a point covers exactly one cell"
        );

        // Zero width: one column, every row.
        let column = visible_cells([centre[0], south, centre[0], north]);
        assert_eq!(column.col_start, column.col_end);
        assert_eq!((column.row_start, column.row_end), (0, GRID_ROWS - 1));

        // Zero height: one row, every column.
        let row = visible_cells([west, centre[1], east, centre[1]]);
        assert_eq!((row.col_start, row.col_end), (0, GRID_COLS - 1));
        assert_eq!(row.row_start, row.row_end);

        // The far edges are grid lines: a viewport sitting on one touches the edge cell.
        let far = visible_cells([east, south, east, north]);
        assert!(!far.is_empty());
        assert_eq!(far.col_start, GRID_COLS - 1);
        let top = visible_cells([west, north, east, north]);
        assert!(!top.is_empty());
        assert_eq!(top.row_end, GRID_ROWS - 1);
    }

    #[test]
    fn a_viewport_with_no_finite_edge_covers_no_cell() {
        let [west, south, east, north] = mercator_bounds();
        for edge in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for slot in 0..4 {
                let mut visible = [west, south, east, north];
                visible[slot] = edge;
                assert!(
                    visible_cells(visible).is_empty(),
                    "{visible:?} has a {edge} edge",
                );
            }
        }
    }

    #[test]
    fn a_cell_range_stops_at_the_cell_the_high_edge_ends_in() {
        // One cell per unit over `0..10`, so every edge below is exact in `f32`: this is
        // the arithmetic the row and column axes are both read with, without the
        // projection's rounding on top of it.
        assert_eq!(cell_range(0.0, 5.0, 0.0, 10.0, 10), Some((0, 4)));
        assert_eq!(cell_range(5.0, 10.0, 0.0, 10.0, 10), Some((5, 9)));
        // A degenerate range on a grid line is the cell before it, and on the far edge
        // the edge cell itself — never one that does not exist.
        assert_eq!(cell_range(5.0, 5.0, 0.0, 10.0, 10), Some((5, 5)));
        assert_eq!(cell_range(10.0, 10.0, 0.0, 10.0, 10), Some((9, 9)));
        // A range that reaches in from outside is clamped to the edge cell.
        assert_eq!(cell_range(-5.0, 0.0, 0.0, 10.0, 10), Some((0, 0)));
        assert_eq!(cell_range(9.0, 15.0, 0.0, 10.0, 10), Some((9, 9)));
        // A range that misses the map, or runs backwards, covers nothing.
        assert_eq!(cell_range(-5.0, -1.0, 0.0, 10.0, 10), None);
        assert_eq!(cell_range(10.5, 11.0, 0.0, 10.0, 10), None);
        assert_eq!(cell_range(4.0, 3.0, 0.0, 10.0, 10), None);
        assert_eq!(cell_range(f32::NAN, 3.0, 0.0, 10.0, 10), None);
        assert_eq!(cell_range(0.0, f32::INFINITY, 0.0, 10.0, 10), None);
    }

    #[test]
    fn an_empty_span_draws_nothing_and_stays_empty_when_grown() {
        assert!(CellSpan::EMPTY.is_empty());
        assert_eq!(CellSpan::EMPTY.indices().count(), 0);
        assert_eq!(CellSpan::EMPTY.grown(), CellSpan::EMPTY);

        for empty in [
            CellSpan {
                col_start: 5,
                col_end: 4,
                row_start: 0,
                row_end: 3,
            },
            CellSpan {
                col_start: 0,
                col_end: 3,
                row_start: 5,
                row_end: 4,
            },
        ] {
            assert!(empty.is_empty(), "a start past its end covers nothing");
            assert_eq!(empty.indices().count(), 0);
            assert_eq!(empty.grown(), empty);
        }

        // `start == end` is one cell, not none.
        let one = CellSpan {
            col_start: 7,
            col_end: 7,
            row_start: 9,
            row_end: 9,
        };
        assert!(!one.is_empty());
        assert_eq!(one.indices().collect::<Vec<_>>(), [9 * GRID_COLS + 7]);
    }

    #[test]
    fn the_rows_are_latitude_bands_not_mercator_bands() {
        // The asset's cell table was cut in degrees, so the row axis is read through
        // the projection. Over this box the two axes differ by up to a quarter of a
        // row, which is what the conversion is for.
        let [west, south, east, north] = mercator_bounds();
        let visible = [
            west,
            project(MAP_WEST, 25.0)[1],
            east,
            project(MAP_WEST, 26.0)[1],
        ];
        let span = visible_cells(visible);
        let first =
            ((25.0 - MAP_SOUTH) / (MAP_NORTH - MAP_SOUTH) * GRID_ROWS as f32).floor() as u32;
        let last =
            (((26.0 - MAP_SOUTH) / (MAP_NORTH - MAP_SOUTH) * GRID_ROWS as f32).ceil() - 1.0) as u32;
        assert_eq!((span.row_start, span.row_end), (first, last));
        assert_eq!((span.col_start, span.col_end), (0, GRID_COLS - 1));

        let mut worst = 0.0f32;
        for step in 0..=100 {
            let latitude = MAP_SOUTH + (MAP_NORTH - MAP_SOUTH) * step as f32 / 100.0;
            let by_latitude = (latitude - MAP_SOUTH) / (MAP_NORTH - MAP_SOUTH) * GRID_ROWS as f32;
            let by_mercator =
                (project(MAP_WEST, latitude)[1] - south) / (north - south) * GRID_ROWS as f32;
            worst = worst.max((by_mercator - by_latitude).abs());
        }
        assert!(
            worst > 0.2,
            "the two row axes only differ by {worst} rows; the conversion is untested",
        );
    }

    #[test]
    fn the_latitude_axis_inverts_the_projection() {
        // `visible_cells` reads the row axis back through the projection, so the
        // conversion has to be the exact inverse at both edges of the box and monotone in
        // between: a step that went backwards would drop a row from a viewport.
        for latitude in [MAP_SOUTH, MAP_NORTH] {
            let [_, y] = project(MAP_WEST, latitude);
            let back = latitude_of(y);
            assert!(
                (back - latitude).abs() < 1e-5,
                "{latitude}° projects to {y} and reads back as {back}°",
            );
        }

        let mut previous = f32::NEG_INFINITY;
        let mut worst = 0.0f32;
        for step in 0..=1_000 {
            let latitude = MAP_SOUTH + (MAP_NORTH - MAP_SOUTH) * step as f32 / 1_000.0;
            let [_, y] = project(MAP_WEST, latitude);
            let back = latitude_of(y);
            assert!(
                back > previous,
                "the inverse Mercator goes backwards at {latitude}°",
            );
            previous = back;
            worst = worst.max((back - latitude).abs());
        }
        // 7.6e-6° measured: a three-hundredth of the 0.2375° a row spans, and far below
        // the 0.2199° a column spans, so no edge can fall on the wrong side of a line.
        assert!(worst < 2e-5, "the inverse Mercator is {worst}° off");
    }

    #[test]
    fn a_span_grows_by_one_cell_and_clamps_to_the_grid() {
        let span = CellSpan {
            col_start: 10,
            col_end: 20,
            row_start: 5,
            row_end: 6,
        };
        assert_eq!(
            span.grown(),
            CellSpan {
                col_start: 9,
                col_end: 21,
                row_start: 4,
                row_end: 7,
            },
        );

        assert_eq!(
            whole_grid().grown(),
            whole_grid(),
            "the margin cannot leave the grid"
        );
        assert_eq!(CellSpan::EMPTY.grown(), CellSpan::EMPTY);
    }

    #[test]
    fn the_span_indices_are_row_major() {
        let span = CellSpan {
            col_start: 62,
            col_end: 63,
            row_start: 30,
            row_end: 31,
        };
        assert_eq!(
            span.indices().collect::<Vec<_>>(),
            [
                30 * GRID_COLS + 62,
                30 * GRID_COLS + 63,
                31 * GRID_COLS + 62,
                31 * GRID_COLS + 63,
            ],
            "the asset's cell tables are keyed row-major",
        );
    }

    /// A road piece with the given flags and length, for the filter tests.
    fn road_piece(flags: u8, length_m: u32) -> Piece {
        Piece {
            start: 0,
            len: 2,
            flags,
            length_m,
        }
    }

    /// The build's fine-copy tolerance, in degrees.
    ///
    /// `assets/build_map.py` writes `ROAD_SIMPLIFY = 0.002`; the primary roads are
    /// packed by the same simplification. Only the pixel-budget test needs it: the
    /// runtime's coarse and middle thresholds are derived from their own tolerances,
    /// while the fine copy's error is checked here against the same 1.2 px budget.
    const ROAD_FINE_TOLERANCE_DEGREES: f32 = 0.002;

    #[test]
    fn the_road_filter_switches_at_its_scale_and_zoom() {
        // 0.008° is 890.6 m on the latitude axis, and 890.6 / 1.2 = 742.1 m/px:
        // the coarse copy is drawn at every scale at least that coarse.
        assert!(
            (ROAD_COARSE_METRES_PER_PIXEL - 742.1).abs() < 0.5,
            "the coarse threshold is {ROAD_COARSE_METRES_PER_PIXEL} m/px",
        );
        // 0.004° is 445.3 m, and 445.3 / 1.2 = 371.1 m/px: the middle copy starts
        // there and is drawn until the coarse copy takes over.
        assert!(
            (ROAD_MIDDLE_METRES_PER_PIXEL - 371.1).abs() < 0.5,
            "the middle threshold is {ROAD_MIDDLE_METRES_PER_PIXEL} m/px",
        );
        assert_eq!(ROAD_PRIMARY_ZOOM, MAX_ZOOM);

        // 1× on the design pane is 1,004 m/px, so it draws the coarse copy; a wide
        // 2,048 px pane is 688 m/px there, below the 742 threshold but above the middle
        // one, so it draws the middle copy.
        assert_eq!(road_filter(1_004.0, 1.0), RoadFilter::CoarseCopy);
        assert_eq!(road_filter(688.0, 1.0), RoadFilter::MiddleCopy);
        assert_eq!(
            road_filter(ROAD_COARSE_METRES_PER_PIXEL, 1.0),
            RoadFilter::CoarseCopy,
            "the threshold itself belongs to the coarse copy",
        );
        assert_eq!(
            road_filter(ROAD_COARSE_METRES_PER_PIXEL - 0.001, 1.0),
            RoadFilter::MiddleCopy,
            "one step finer than the coarse threshold is the middle copy",
        );
        assert_eq!(
            road_filter(ROAD_MIDDLE_METRES_PER_PIXEL, 1.0),
            RoadFilter::MiddleCopy,
            "the middle threshold itself belongs to the middle copy",
        );
        assert_eq!(
            road_filter(ROAD_MIDDLE_METRES_PER_PIXEL - 0.001, 1.0),
            RoadFilter::FineCopy,
            "one step finer than the middle threshold is the fine copy",
        );
        assert_eq!(road_filter(400.0, 2.0), RoadFilter::MiddleCopy);
        assert_eq!(road_filter(359.0, 2.0), RoadFilter::FineCopy);
        assert_eq!(
            road_filter(359.0, 3.5),
            RoadFilter::FineCopy,
            "3.5× is not the closest zoom: the primary roads wait for 4×",
        );
        assert_eq!(
            road_filter(359.0, 4.0),
            RoadFilter::FineCopyAndPrimary,
            "the primary roads join at the closest zoom, whatever the scale",
        );
        assert_eq!(road_filter(251.0, 4.0), RoadFilter::FineCopyAndPrimary);
        assert_eq!(road_filter(251.0, 3.5), RoadFilter::FineCopy);
        // A narrow pane at 4× is 598 m/px: the middle copy and the primary roads
        // coexist there, which is why the filter has a variant for that pair.
        assert_eq!(road_filter(598.0, 4.0), RoadFilter::MiddleCopyAndPrimary);
        assert_eq!(road_filter(598.0, 3.5), RoadFilter::MiddleCopy);

        // A scale that is not a positive number is a pane that is not laid out yet: it
        // has no error budget, so it reads as the finest copy. The primary layer is
        // still the zoom's decision.
        assert_eq!(road_filter(0.0, 1.0), RoadFilter::FineCopy);
        assert_eq!(road_filter(0.0, 4.0), RoadFilter::FineCopyAndPrimary);
        assert_eq!(road_filter(f32::NAN, 1.0), RoadFilter::FineCopy);
        assert_eq!(road_filter(f32::NEG_INFINITY, 1.0), RoadFilter::FineCopy);
        assert_eq!(
            road_filter(f32::INFINITY, 1.0),
            RoadFilter::CoarseCopy,
            "an infinite scale cannot come out of `metres_per_pixel`, and if it did the \
             coarsest copy would still be the one it justifies",
        );
    }

    /// The road thresholds are the build's tolerances turned into a pixel budget, and
    /// the pixel budget is the one the plan sets.
    ///
    /// A tolerance is isotropic in degrees, so both axes have to be checked: a degree of
    /// latitude is 111,320 m, while a degree of longitude at the map's scale latitude is
    /// 100,160 m. The latitude axis is the worst one, and a threshold derived from the
    /// longitude axis alone would pass this test while being 11% too permissive.
    #[test]
    fn every_road_copy_stays_inside_the_pixel_budget_where_it_is_switched_in() {
        // One pixel at a scale of 1 is one Mercator radian, so a degree of longitude is
        // π/180 pixels and covers `metres_per_pixel(1.0) × π/180` metres.
        let metres_per_degree_longitude = metres_per_pixel(1.0) * std::f32::consts::PI / 180.0;
        let metres_per_degree_latitude = 111_320.0;
        assert!(
            metres_per_degree_latitude > metres_per_degree_longitude,
            "the latitude axis is the worst one: {metres_per_degree_latitude} vs \
             {metres_per_degree_longitude}",
        );
        assert_eq!(
            ROAD_METRES_PER_DEGREE, metres_per_degree_latitude,
            "the threshold has to be derived from the worst axis",
        );

        // The coarse copy is drawn at every scale at least its threshold, so its worst
        // error is at the threshold itself: 890.6 / 742.1 = 1.20 px on the latitude
        // axis, 801.3 / 742.1 = 1.08 px on the longitude one.
        let coarse_latitude = ROAD_COARSE_TOLERANCE_DEGREES * metres_per_degree_latitude
            / ROAD_COARSE_METRES_PER_PIXEL;
        let coarse_longitude = ROAD_COARSE_TOLERANCE_DEGREES * metres_per_degree_longitude
            / ROAD_COARSE_METRES_PER_PIXEL;
        assert!(coarse_latitude >= coarse_longitude);
        for (axis, error_px) in [
            ("latitude", coarse_latitude),
            ("longitude", coarse_longitude),
        ] {
            assert!(
                error_px <= ROAD_WORST_CASE_AT_THRESHOLD_PX,
                "the coarse copy is {error_px:.2} px off on the {axis} axis at its \
                 {ROAD_COARSE_METRES_PER_PIXEL:.1} m/px switch, over the \
                 {ROAD_WORST_CASE_AT_THRESHOLD_PX} px budget",
            );
        }

        // The middle copy is drawn at every scale from its threshold up to the coarse
        // one, so its worst error is at its own threshold: 445.3 / 371.1 = 1.20 px on
        // the latitude axis, 400.6 / 371.1 = 1.08 px on the longitude one.
        let middle_latitude = ROAD_MIDDLE_TOLERANCE_DEGREES * metres_per_degree_latitude
            / ROAD_MIDDLE_METRES_PER_PIXEL;
        let middle_longitude = ROAD_MIDDLE_TOLERANCE_DEGREES * metres_per_degree_longitude
            / ROAD_MIDDLE_METRES_PER_PIXEL;
        assert!(middle_latitude >= middle_longitude);
        for (axis, error_px) in [
            ("latitude", middle_latitude),
            ("longitude", middle_longitude),
        ] {
            assert!(
                error_px <= ROAD_WORST_CASE_AT_THRESHOLD_PX,
                "the middle copy is {error_px:.2} px off on the {axis} axis at its \
                 {ROAD_MIDDLE_METRES_PER_PIXEL:.1} m/px switch, over the \
                 {ROAD_WORST_CASE_AT_THRESHOLD_PX} px budget",
            );
        }

        // The fine copy starts at the middle threshold and is still drawn at the primary
        // layer's switch, 4× on the design pane (251 m/px), so the closer of the two is
        // its worst case: 222.6 / 251 = 0.89 px on the latitude axis.
        let primary_switch = Projection::new(
            Camera {
                zoom: ROAD_PRIMARY_ZOOM,
                ..Camera::default()
            },
            DESIGN_VIEWPORT,
        )
        .metres_per_pixel();
        assert!(
            (primary_switch - 251.0).abs() < 1.0,
            "the design pane's 4× is {primary_switch} m/px",
        );
        for threshold in [ROAD_MIDDLE_METRES_PER_PIXEL, primary_switch] {
            let fine_latitude =
                ROAD_FINE_TOLERANCE_DEGREES * metres_per_degree_latitude / threshold;
            let fine_longitude =
                ROAD_FINE_TOLERANCE_DEGREES * metres_per_degree_longitude / threshold;
            assert!(fine_latitude >= fine_longitude);
            for (axis, error_px) in [("latitude", fine_latitude), ("longitude", fine_longitude)] {
                assert!(
                    error_px <= ROAD_WORST_CASE_AT_THRESHOLD_PX,
                    "the fine copy is {error_px:.2} px off on the {axis} axis at \
                     {threshold:.1} m/px, over the {ROAD_WORST_CASE_AT_THRESHOLD_PX} px \
                     budget",
                );
            }
        }
    }

    #[test]
    fn a_finer_scale_never_draws_a_coarser_road_copy() {
        // Monotone in metres per pixel: at a fixed zoom, a finer scale (fewer metres per
        // pixel) may only move towards the more detailed filter, so the answer walks
        // coarse → middle → fine (and gains the primary roads once the zoom is close
        // enough) and never back.
        let rank = |filter: RoadFilter| match filter {
            RoadFilter::CoarseCopy => 0,
            RoadFilter::MiddleCopy | RoadFilter::MiddleCopyAndPrimary => 1,
            RoadFilter::FineCopy | RoadFilter::FineCopyAndPrimary => 2,
        };
        for (zoom, expected) in [(2.0, 2), (ROAD_PRIMARY_ZOOM, 2)] {
            let mut previous = rank(road_filter(f32::INFINITY, zoom));
            assert_eq!(previous, 0);
            for step in (1..=2_000).rev() {
                let metres = step as f32 * 5.0;
                let current = rank(road_filter(metres, zoom));
                assert!(
                    current >= previous,
                    "{metres} m/px at {zoom}× reads as rank {current} after rank {previous}",
                );
                previous = current;
            }
            assert_eq!(previous, expected, "5 m/px at {zoom}× is the finest filter");
        }
    }

    #[test]
    fn each_road_filter_keeps_the_pieces_it_names() {
        // The three copies of class 0 and the primary layer, as the build packs them.
        let motorway = road_piece(0, 40);
        let middle = road_piece(ROAD_MIDDLE, 0);
        let coarse = road_piece(ROAD_COARSE, 0);
        let primary = road_piece(ROAD_PRIMARY, 900);
        // Combinations the build does not write, but the decoder still has to answer for.
        let coarse_primary = road_piece(ROAD_COARSE | ROAD_PRIMARY, 0);
        let middle_primary = road_piece(ROAD_MIDDLE | ROAD_PRIMARY, 0);

        // Coarse filter: only the coarse copy, and not the primary layer — the coarse
        // copy is a fitted-zoom picture and primary waits for the closest zoom.
        assert_eq!(road_path(RoadFilter::CoarseCopy, &coarse, 0.0), Some(0));
        assert_eq!(
            road_path(RoadFilter::CoarseCopy, &coarse_primary, 0.0),
            Some(0),
            "the coarse copy is class 0 whether or not a bit says otherwise",
        );
        assert_eq!(
            road_path(RoadFilter::CoarseCopy, &motorway, 0.0),
            None,
            "the fine copy is not drawn at a coarse scale",
        );
        assert_eq!(road_path(RoadFilter::CoarseCopy, &middle, 0.0), None);
        assert_eq!(road_path(RoadFilter::CoarseCopy, &primary, 0.0), None);

        // Middle filter: only the middle copy, no primary.
        assert_eq!(road_path(RoadFilter::MiddleCopy, &middle, 0.0), Some(0));
        assert_eq!(
            road_path(RoadFilter::MiddleCopy, &middle_primary, 0.0),
            Some(0)
        );
        assert_eq!(road_path(RoadFilter::MiddleCopy, &motorway, 0.0), None);
        assert_eq!(road_path(RoadFilter::MiddleCopy, &coarse, 0.0), None);
        assert_eq!(road_path(RoadFilter::MiddleCopy, &primary, 0.0), None);

        // Middle + primary: the middle class-0 copy, and the primary layer in path 1.
        assert_eq!(
            road_path(RoadFilter::MiddleCopyAndPrimary, &middle, 0.0),
            Some(0)
        );
        assert_eq!(
            road_path(RoadFilter::MiddleCopyAndPrimary, &primary, 0.0),
            Some(1),
            "the closest filter draws primary roads in the second path",
        );
        assert_eq!(
            road_path(RoadFilter::MiddleCopyAndPrimary, &middle_primary, 0.0),
            Some(0)
        );
        assert_eq!(
            road_path(RoadFilter::MiddleCopyAndPrimary, &motorway, 0.0),
            None
        );
        assert_eq!(
            road_path(RoadFilter::MiddleCopyAndPrimary, &coarse, 0.0),
            None
        );

        // Fine filter: every class-0 piece the fine copy holds, no primary.
        assert_eq!(
            road_path(RoadFilter::FineCopy, &motorway, 0.0),
            Some(0),
            "the fine copy keeps a piece with no length",
        );
        assert_eq!(
            road_path(RoadFilter::FineCopy, &primary, 0.0),
            None,
            "the fine filter is class 0 only",
        );
        assert_eq!(road_path(RoadFilter::FineCopy, &middle, 0.0), None);
        assert_eq!(
            road_path(RoadFilter::FineCopy, &coarse, 0.0),
            None,
            "the coarse copy is not drawn at or below its threshold",
        );

        // Fine + primary.
        assert_eq!(
            road_path(RoadFilter::FineCopyAndPrimary, &motorway, 0.0),
            Some(0)
        );
        assert_eq!(
            road_path(RoadFilter::FineCopyAndPrimary, &primary, 0.0),
            Some(1),
            "the closest filter draws primary roads in the second path",
        );
        assert_eq!(
            road_path(RoadFilter::FineCopyAndPrimary, &middle, 0.0),
            None,
            "and no two copies are drawn on top of each other",
        );
        assert_eq!(
            road_path(RoadFilter::FineCopyAndPrimary, &coarse, 0.0),
            None
        );
    }

    /// A piece of a short chain is drawn only once a pixel is fine enough; a piece of a
    /// long one is unaffected by that gate.
    #[test]
    fn a_short_chain_is_drawn_only_once_a_pixel_is_fine_enough() {
        let short = road_piece(ROAD_SHORT_CHAIN, 0);
        let long = road_piece(0, 0);

        // The design pane's fitted 1× is 1,004 m/px and its 1.5× is 669 m/px: both are
        // coarser than the gate, so the short chain is not drawn.
        for metres_per_pixel in [1_004.0, 669.0, 628.001] {
            assert_eq!(
                road_path(RoadFilter::FineCopy, &short, metres_per_pixel),
                None,
                "a short chain at {metres_per_pixel} m/px",
            );
            assert_eq!(
                road_path(RoadFilter::FineCopy, &long, metres_per_pixel),
                Some(0),
                "a long chain at {metres_per_pixel} m/px",
            );
        }

        // 628 m/px is just below the design pane's 1.6× (627.5 m/px), so the chain is
        // back there and stays drawn at every finer scale.
        for metres_per_pixel in [628.0, 627.0, 251.0, 0.0] {
            assert_eq!(
                road_path(RoadFilter::FineCopy, &short, metres_per_pixel),
                Some(0),
                "a short chain at {metres_per_pixel} m/px",
            );
        }

        // The gate is independent of the copy bit: the decoder must drop a coarse piece
        // carrying it too, even though the build does not write that combination.
        let coarse_short = road_piece(ROAD_COARSE | ROAD_SHORT_CHAIN, 0);
        assert_eq!(
            road_path(RoadFilter::CoarseCopy, &coarse_short, 1_004.0),
            None
        );
        assert_eq!(
            road_path(RoadFilter::CoarseCopy, &coarse_short, 627.0),
            Some(0)
        );
    }

    /// The pane the design measures on: the user's 1.56:1 viewport.
    const DESIGN_VIEWPORT: [f32; 2] = [1_404.0, 900.0];

    #[test]
    fn the_land_tier_switches_at_its_three_thresholds() {
        assert_eq!(LAND_TIER_FINE_METRES_PER_PIXEL, 288.0);
        assert_eq!(LAND_TIER_MIDDLE_METRES_PER_PIXEL, 450.0);
        assert_eq!(LAND_TIER_COARSE_METRES_PER_PIXEL, 911.0);

        // Below the 0.002° threshold the finest coast is the only one whose error is
        // under a pixel.
        assert_eq!(land_tier(0.0), 0);
        assert_eq!(land_tier(1.0), 0);
        assert_eq!(land_tier(287.999), 0);
        assert_eq!(
            land_tier(LAND_TIER_FINE_METRES_PER_PIXEL),
            1,
            "the threshold itself belongs to the 0.002° tier",
        );
        assert_eq!(land_tier(300.0), 1);
        assert_eq!(land_tier(449.999), 1);
        assert_eq!(
            land_tier(LAND_TIER_MIDDLE_METRES_PER_PIXEL),
            2,
            "the 0.003° tier starts at the middle threshold",
        );
        assert_eq!(land_tier(700.0), 2);
        assert_eq!(land_tier(910.999), 2);
        assert_eq!(
            land_tier(LAND_TIER_COARSE_METRES_PER_PIXEL),
            3,
            "and the coarsest at the last one",
        );
        assert_eq!(land_tier(5_000.0), 3);

        // A scale that is not a positive number is a pane that is not laid out yet: it
        // has no error budget, so it reads as the finest coast.
        assert_eq!(land_tier(f32::NAN), 0);
        assert_eq!(land_tier(f32::NEG_INFINITY), 0);
        assert_eq!(
            land_tier(f32::INFINITY),
            3,
            "an infinite scale cannot come out of `metres_per_pixel`, and if it did the \
             coarsest coast would still be the one it justifies",
        );
    }

    /// The simplification tolerance of each land tier, finest first, in degrees.
    ///
    /// `assets/build_map.py` writes `LAND_TIERS`; these are the same four values. They
    /// are pinned here so the threshold test can name the geometry the thresholds were
    /// derived from, and a build whose tolerances drift is a failure rather than a
    /// silent mismatch.
    const LAND_TIER_TOLERANCES_DEGREES: [f32; LAND_TIERS] = [0.001, 0.002, 0.003, 0.006];

    /// The measured worst distance each tier's drawn coast sits from tier 0's, in metres.
    ///
    /// `assets/build_map.py` prints the discrete Hausdorff distance between each tier's
    /// **boundary** and tier 0's on the same clipped land, on the local equirectangular
    /// plane `length_metres` uses; these are those measurements on this asset:
    /// **344.8 m at 0.002°**, 503.4 m at 0.003° and 1,092.9 m at 0.006°. The first
    /// pass printed 312.4 / 456.5 / 1,043.4 m because it scaled the degree difference by
    /// the longitude axis; a mostly north-south pair is underestimated by that. Tier 0
    /// is the reference every measurement is taken against, so it is zero. They are
    /// measurements, not formulas: **changing `LAND_TIERS` (or any tier's tolerance)
    /// means re-running the build and updating this table**, and the threshold test
    /// below fails if a new number outgrows the pixel budget.
    const LAND_TIER_WORST_CASE_METRES: [f32; LAND_TIERS] = [0.0, 344.8, 503.4, 1_092.9];

    /// The most a land tier may be off at the scale where it starts being drawn, in
    /// pixels.
    ///
    /// 1.0 is the ideal. The 0.006° tier measures 1.1996 px at its 911 m/px switch —
    /// 20% over — and that is accepted: forcing it under 1.0 would mean switching to
    /// the 0.003° tier there, which costs about 4,000 more triangles and pushes the
    /// frame back into GPUI's 2^17 instance-buffer step, doubling that memory.
    const LAND_TIER_WORST_CASE_AT_THRESHOLD_PX: f32 = 1.2;

    /// The most the tier drawn at the design pane's fitted zoom may be off, in pixels.
    ///
    /// 1.0 is the ideal. The 0.006° tier measures 1.089 px at 1,004 m/px — the 9% the
    /// plan's one-pixel rule gives up so the fitted frame stays in the 2^16
    /// instance-buffer step; the 0.003° tier would cost ~4,000 more triangles for that
    /// 9%. It is a worst-case single point, not typical displacement.
    const LAND_TIER_WORST_CASE_AT_FIT_PX: f32 = 1.1;

    /// The measured error of every tier at the two scales that matter: where it is
    /// switched in, and the design pane's fitted zoom.
    ///
    /// This is the plan's §8 one-pixel rule expressed as a test: a tier is never more
    /// than [`LAND_TIER_WORST_CASE_AT_THRESHOLD_PX`] off where it starts being drawn,
    /// and the tier the fitted view uses is never more than
    /// [`LAND_TIER_WORST_CASE_AT_FIT_PX`] off. Both are measured constants, so moving
    /// a threshold, a tier's tolerance or the build's simplification shows up here
    /// instead of being absorbed silently.
    ///
    /// The threshold rule itself is checked, not just the current numbers: **every
    /// threshold is at least the tier's measured worst error divided by 1.2**. The
    /// 288 and 911 thresholds are the exact ceils of that division; 450 is a round-up
    /// of 419.5, so it is the one threshold that leaves slack (1.119 px at the switch
    /// rather than 1.2). Tiers 1 and 3 are the tight cases at 1.197 and 1.1996 px, so
    /// their thresholds cannot be lowered without failing.
    #[test]
    fn every_tier_stays_inside_the_pixel_budget_the_plan_sets() {
        for (threshold, tier) in [
            (LAND_TIER_FINE_METRES_PER_PIXEL, 1),
            (LAND_TIER_MIDDLE_METRES_PER_PIXEL, 2),
            (LAND_TIER_COARSE_METRES_PER_PIXEL, 3),
        ] {
            let derived = LAND_TIER_WORST_CASE_METRES[tier] / LAND_TIER_WORST_CASE_AT_THRESHOLD_PX;
            assert!(
                threshold >= derived,
                "tier {tier}'s {threshold} m/px switch is finer than its {:.1} m worst \
                 error divided by {LAND_TIER_WORST_CASE_AT_THRESHOLD_PX} px ({derived:.1})",
                LAND_TIER_WORST_CASE_METRES[tier],
            );
            let error_px = LAND_TIER_WORST_CASE_METRES[tier] / threshold;
            assert!(
                error_px <= LAND_TIER_WORST_CASE_AT_THRESHOLD_PX,
                "tier {tier} is {error_px:.2} px off at its {threshold} m/px switch, over \
                 the {LAND_TIER_WORST_CASE_AT_THRESHOLD_PX} px budget",
            );
        }

        // The fitted zoom on the design pane draws 1 px at 1,004 m; the tier it picks
        // there is the 0.006° one, whose worst point is 1,092.9 m = 1.089 px.
        let fitted = land_tier(1_004.0);
        let error_px = LAND_TIER_WORST_CASE_METRES[fitted] / 1_004.0;
        assert!(
            error_px <= LAND_TIER_WORST_CASE_AT_FIT_PX,
            "the fitted view draws tier {fitted}, which is {error_px:.2} px off, over the \
             {LAND_TIER_WORST_CASE_AT_FIT_PX} px budget",
        );
        assert_eq!(
            fitted,
            LAND_TIERS - 1,
            "the fitted view draws the coarsest tier"
        );

        // The four tolerances are the asset's own, in the order the tiers are stored.
        assert_eq!(
            LAND_TIER_TOLERANCES_DEGREES,
            [0.001, 0.002, 0.003, 0.006],
            "the tolerances the thresholds were derived from",
        );
    }

    #[test]
    fn a_coarser_scale_never_draws_a_finer_coast() {
        // Monotone in metres per pixel: a coarser scale (more metres per pixel) may only
        // move towards the coarser tiers, so the answer walks 0 → 1 → 2 → 3 and never
        // back.
        let mut previous = land_tier(0.0);
        assert_eq!(previous, 0);
        for step in 1..=2_000 {
            let metres = step as f32 * 5.0;
            let tier = land_tier(metres);
            assert!(
                tier >= previous,
                "{metres} m/px reads as tier {tier} after tier {previous}",
            );
            assert!(tier < LAND_TIERS, "a tier index outside the asset");
            previous = tier;
        }
        assert_eq!(previous, LAND_TIERS - 1, "10 km/px is the coarsest tier");
    }

    #[test]
    fn the_zoom_selects_the_tier_by_the_scale_it_draws_at() {
        // The fitted zoom on the design's pane is 1 px ≈ 1 km, so the 0.006° coast is
        // the one whose error is a pixel there; 1.6× is 1 px ≈ 627 m, where the 0.003°
        // coast fits; 2.8× is 359 m/px, under the 450 m/px middle threshold but still
        // over the 0.002° one's 288 m/px, so it draws the 0.002° coast; 4× is 251 m/px
        // and draws the 0.001° coast.
        let at = |zoom: f32| {
            Projection::new(
                Camera {
                    center: map_centre(),
                    zoom,
                }
                .clamped(DESIGN_VIEWPORT),
                DESIGN_VIEWPORT,
            )
            .metres_per_pixel()
        };
        let fitted = at(MIN_ZOOM);
        assert!(
            (900.0..=1_100.0).contains(&fitted),
            "the fitted zoom is {fitted} m/px, not about a kilometre",
        );
        for (zoom, tier) in [(MIN_ZOOM, 3), (1.6, 2), (2.8, 1), (MAX_ZOOM, 0)] {
            assert_eq!(
                land_tier(at(zoom)),
                tier,
                "at {zoom}× the pane draws {:.0} m/px",
                at(zoom),
            );
        }

        // The scale is the projection's, not the zoom number's: a narrow pane covers
        // more ground per pixel at the same zoom, so the same 4× reads as a coarser
        // tier there than on the design's pane.
        let narrow = [320.0, 400.0];
        let at_narrow = |zoom: f32| {
            Projection::new(
                Camera {
                    center: map_centre(),
                    zoom,
                }
                .clamped(narrow),
                narrow,
            )
            .metres_per_pixel()
        };
        assert!(
            at_narrow(MAX_ZOOM) > at(MAX_ZOOM),
            "a narrow pane at 4× draws {:.0} m/px, no finer than the design pane's {:.0}",
            at_narrow(MAX_ZOOM),
            at(MAX_ZOOM),
        );
        assert_eq!(
            (land_tier(at_narrow(MAX_ZOOM)), land_tier(at(MAX_ZOOM))),
            (2, 0),
            "the same zoom reads as two different tiers on two different panes",
        );

        // And the fitted zoom itself is not one tier: a 2,048 px wide pane shows the
        // same degrees across more pixels, so its fitted view is finer than the design
        // pane's.
        let wide_fitted = Projection::new(Camera::default(), [2_048.0, 900.0]).metres_per_pixel();
        assert!(
            wide_fitted < fitted,
            "a wider pane is finer at the same zoom"
        );
        assert_eq!(
            (land_tier(wide_fitted), land_tier(fitted)),
            (2, 3),
            "1× is not one tier either: {wide_fitted:.0} vs {fitted:.0} m/px",
        );
    }

    /// The scale the picture is drawn at, pinned at four independent viewports.
    ///
    /// These are the numbers the thresholds are compared against, so they are asserted
    /// as plain values rather than recomputed from the same constants the function uses
    /// (which would only prove the formula equals itself). 1,404 × 900 is the design
    /// pane; 2,048 × 900 is the same height on a wide one; 320 × 400 is the narrowest
    /// pane a widget is laid out in, at the fitted zoom and at four times it.
    #[test]
    fn the_metres_per_pixel_is_the_scale_the_picture_is_drawn_at() {
        for (viewport, zoom, expected) in [
            ([1_404.0, 900.0], 1.0, 1_004.0),
            ([2_048.0, 900.0], 1.0, 688.0),
            ([320.0, 400.0], 1.0, 2_394.0),
            ([320.0, 400.0], 4.0, 598.0),
        ] {
            let projection = Projection::new(
                Camera {
                    center: map_centre(),
                    zoom,
                }
                .clamped(viewport),
                viewport,
            );
            let metres = projection.metres_per_pixel();
            assert!(
                (metres - expected).abs() < 5.0,
                "{viewport:?} at {zoom}× draws {metres:.1} m/px, not {expected}",
            );
        }

        // A pane with no size has no scale to read, and a scale that is not a finite
        // positive number is not a scale at all.
        assert_eq!(
            Projection::new(Camera::default(), [0.0, 0.0]).metres_per_pixel(),
            0.0,
        );
        for scale in [0.0, -1.0, f32::NAN, f32::NEG_INFINITY, f32::INFINITY] {
            assert_eq!(metres_per_pixel(scale), 0.0, "{scale} is not a scale");
        }
    }
}
