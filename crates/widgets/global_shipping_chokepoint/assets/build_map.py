"""Pack the OSM sources into `hormuz_map.bin`, the KHL2 map asset.

Run `download_map.py` first: this script reads the four sources next to itself
and fails with an actionable message when one is missing.

    python crates/widgets/global_shipping_chokepoint/assets/build_map.py

What is packed (all little-endian, see `README.md` for the format):

1. **land, four tiers** — OSM land polygons, clipped to the map box once, then
   simplified at 0.001° (~100 m on the ground here, the visual reference),
   0.002° (~200 m), 0.003° (~300 m) and 0.006° (~600 m). Each tier is simplified
   from the same clipped geometry with `preserve_topology=True`, drops islands
   under 0.01 km² on its own and packs one closed ring per polygon ring. The
   fitted zoom draws the coarsest tier, where one pixel is about a kilometre.
2. **borders** — the member ways of the `boundary=administrative` +
   `admin_level=2` relations, plus any such way that belongs to no relation,
   without `maritime=yes` or `natural=coastline` and with at least one point on
   the drawn land, clipped and simplified to ~55 m, cut per grid cell. The land
   mask is the **tier 0** geometry, so the borders stay where they were.
3. **roads** — `highway=motorway|trunk|primary`. The motorways and trunks are
   joined into the chains their shared endpoints allow and stored **three** times:
   a **fine copy** of every chain at 0.002° (~200 m), a **middle copy** of the
   chains at least `SHORT_CHAIN_KM` long at 0.004° (~400 m), and a **coarse copy**
   of the same long chains at 0.008° (~800 m). The fine copy marks a short chain
   with `FLAG_SHORT_CHAIN`, so the runtime draws it only once a pixel is fine
   enough; the middle and coarse copies leave it out. Primary roads are packed
   once at 0.002°. Every layer is cut per grid cell, with the flag bits the
   runtime LOD filter reads.

The same inputs produce a byte-identical file: every list is sorted before it
is packed.

Requires `shapely>=2.2`, `pyosmium>=4.3` and `pyshp>=2.3`; the sources are
downloaded by `download_map.py`, which is standard library only.
"""

from __future__ import annotations

import argparse
import math
import struct
import sys
import time
import zipfile
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

import osmium
import shapefile
import shapely
from shapely import STRtree, box, make_valid
from shapely.geometry import LineString, Point, Polygon, shape
from shapely.geometry.base import BaseGeometry
from shapely.ops import linemerge, nearest_points, unary_union

# The asset directory is inside the repository: importing `download_map` must not
# leave a `__pycache__` next to the committed files.
sys.dont_write_bytecode = True

import download_map  # noqa: E402  (after the bytecode switch above)

#: The map box: the user's screenshot, extended half a degree north and south so
#: a cover fit never leaves an ocean-coloured band above or below the viewport.
WEST, SOUTH, EAST, NORTH = 47.802, 21.443, 61.875, 30.042

#: The asset grid. Cell `row * GRID_COLS + col`, column 0 at the west edge and
#: row 0 at the south edge.
GRID_COLS, GRID_ROWS = 64, 32

#: The KHL2 version this script writes, and how many sections it packs.
#:
#: Version 3 adds the 0.002° land tier to version 2's three tiers, so the section
#: count and the kinds after the land both move. The parser rejects every other
#: version: there is no migration and no second layout to maintain.
VERSION = 3
SECTION_COUNT = 6

#: Section kinds of the KHL2 asset, in the order the file stores them: the four
#: land tiers coarse-last, then the two line layers. `LAND_KINDS` is the single
#: source for the land kinds; `KIND_BORDERS`/`KIND_ROADS` follow them.
LAND_KINDS = (1, 2, 3, 4)
KIND_BORDERS = LAND_KINDS[-1] + 1
KIND_ROADS = KIND_BORDERS + 1

#: Road flags: bit 0 is "primary", bit 1 is "this piece belongs to the middle
#: copy", bit 2 is "this piece belongs to the coarse copy", bit 3 is "this piece
#: belongs to a class-0 chain shorter than `SHORT_CHAIN_KM`". The runtime rejects a
#: piece whose flags hold any other bit.
FLAG_PRIMARY = 1 << 0
FLAG_MIDDLE = 1 << 1
FLAG_COARSE = 1 << 2
FLAG_SHORT_CHAIN = 1 << 3

#: The road classes the asset keeps, as `highway` tag values.
ROAD_CLASSES = frozenset({"motorway", "trunk", "primary"})

#: The four land tiers, one per section, coarsest last, in degrees.
#:
#: Every tier is simplified from the *same* clipped land, so the four are the
#: same coastline at four resolutions rather than four independent readings of
#: the source. `0.001` is about 100 m on the ground at this latitude, which is
#: what the map drew before the tiers existed and stays the visual reference;
#: `0.006` is about 600 m, and its worst deviation from tier 0 measures 1.04
#: pixels at the fitted zoom (see `src/model.rs`, where the runtime's thresholds
#: are derived from the same measurements). The runtime picks a tier by metres
#: per pixel, never by zoom.
LAND_TIERS = (0.001, 0.002, 0.003, 0.006)
#: Islands smaller than this are dropped, in every tier. The area is converted to
#: km² first: a raw square degree is about 11,200 km² here and would drop almost
#: every island.
LAND_MIN_AREA_KM2 = 0.01
#: The places a coarser tier must not erase on the strait itself, as a point on a
#: piece of land big enough to survive the area filter. Checked by point-in-polygon
#: against every tier's kept polygons and printed by the build.
NAMED_PLACES = {
    "Qeshm": (55.85, 26.85),
    "Hormuz": (56.46, 27.06),
    "Khasab": (56.25, 26.18),
}
#: Borders are simplified to 0.0005° (about 55 m).
BORDER_SIMPLIFY = 0.0005
#: The margin a border point gets when it is tested against the drawn land.
#:
#: The land mask is the tier 0 geometry, simplified to 0.001°, so the drawn
#: coastline sits up to about 100 m off the real one and a border point that is
#: truly on the coast can fall just outside the drawn polygon. Twice that keeps
#: such a point: the Saudi–Qatar connector's nearest point reads 57 m from the
#: drawn coast. The sea-only stretches the mask exists for are a kilometre or
#: more out — the two Abu Dhabi rings read 877 m and 1,486 m — so nothing in
#: between is ambiguous.
BORDER_LAND_MASK_TOLERANCE = 0.002
#: Roads are simplified to 0.002° (about 200 m).
ROAD_SIMPLIFY = 0.002
#: The coarse copy of the motorways and trunks is simplified to 0.008° (about
#: 800 m). One pixel at the fitted zoom is about a kilometre, so the copy is
#: under a pixel off where it is drawn, and its vertices are the only ones a 1×
#: frame pays for.
COARSE_ROAD_SIMPLIFY = 0.008
#: The middle copy is simplified to 0.004° (about 400 m), for the scales between
#: the coarse copy's 742 m/px threshold and the fine copy: 0.004° is 445.3 m on the
#: latitude axis, and 445.3 / 1.2 = 371 m/px is the coarsest scale at which it is
#: still inside the runtime's 1.2 px budget. It is built from the same long chains
#: as the coarse copy, so a short chain is drawn only by the fine copy — at the
#: scales the middle copy covers it would be under a pixel wide.
MIDDLE_ROAD_SIMPLIFY = 0.004
#: Chains shorter than this many kilometres are feature-dropped: the class-0
#: middle and coarse copies leave them out and the fine copy marks them so the
#: runtime draws them only once a pixel is finer than
#: `model::ROAD_SHORT_CHAIN_METRES_PER_PIXEL`; the primary copy leaves them out
#: at build time, because it only draws at the closest zoom. A chain is a whole
#: road between junctions, so dropping one keeps every drawn road continuous;
#: dropping a piece of a longer chain would not. The build prints the
#: distributions this is chosen from.
SHORT_CHAIN_KM = 0.3

#: Degrees to kilometres at the equator; every area scales it by the cosine of
#: the latitude.
DEGREE_KM = 111.32
#: Degrees to metres at the equator, for the build's own length statistics.
#:
#: It is 111,320 m, while the runtime's `metres_per_pixel` uses the mean Earth
#: radius (111,195 m per degree); after both are scaled by the same cosine the
#: build's figure is 0.1125% larger. That is far below the 0.3 km feature drop's
#: resolution, and the runtime's 1.2 px thresholds deliberately do not use it.
DEGREE_M = 111_320.0

#: The latitude the map's scale is quoted at, in degrees: the box's own middle. The
#: runtime reads its metres-per-pixel at the same row (`model::MAP_SCALE_LATITUDE`).
SCALE_LATITUDE = (SOUTH + NORTH) / 2
#: The fitted zoom's scale on the user's 1,404 × 900 pane, in metres per pixel.
#:
#: Documentation only: it turns the Hausdorff distances printed below into the
#: pixels the plan's §8 measures them in. The runtime computes the real figure from
#: the projection, and this is that figure for the pane the design is checked on.
FITTED_METRES_PER_PIXEL = 1_003.95
#: The runtime's worst-error budget at a land tier's switch, in pixels.
#:
#: `src/model.rs` holds the same 1.2 as `LAND_TIER_WORST_CASE_AT_THRESHOLD_PX`; the
#: build prints each tier's derived threshold with it so the number in the model can
#: be checked against the measurement rather than against the nominal tolerance.
TIER_PIXEL_BUDGET = 1.2

#: Below these sizes a source is not a plausible download of that file.
PBF_MIN_BYTES = 20 * 1024**2
LAND_ZIP_MIN_BYTES = 500 * 1024**2

#: The asset budget from the plan: 2 MiB for the whole base map. The three extra
#: land tiers add about 0.2 MiB of points, so four tiers still fit.
MAX_ASSET_BYTES = 2 * 1024 * 1024


@dataclass
class Piece:
    """One packed polyline: a border fragment, a road fragment or a land ring."""

    coords: list[tuple[float, float]]
    flags: int = 0
    length_m: int = 0
    #: The latitude every segment's length is scaled at. A cell cut inherits it, which
    #: makes the cut piece lengths exactly additive: a chain's fragments sum to its own
    #: length at the same reference latitude, so a missing fragment cannot hide.
    reference_latitude: float = 0.0
    #: The chain a road piece came from, as an index into its LOD's chain list. A cell
    #: cut inherits it; it is not packed.
    source: int = -1


@dataclass
class LandTier:
    """One land section: the simplification it was built at and the rings it kept."""

    #: The simplification tolerance, in degrees.
    tolerance: float
    #: The simplified geometry the rings were cut out of; tier 0's is the border mask.
    geometry: BaseGeometry
    #: The kept polygons, before their rings were cut out.
    polygons: int
    #: The dropped polygons, as the count under `LAND_MIN_AREA_KM2` plus the
    #: degenerate rings `make_valid` can leave at a coarser tolerance.
    dropped: int
    #: The largest polygon this tier's own area filter dropped, in km².
    largest_dropped_km2: float
    #: Where that polygon sat, as longitude and latitude in degrees.
    largest_dropped_at: tuple[float, float]
    #: The closed rings, one per exterior or interior ring of a kept polygon.
    rings: list[Piece]
    #: The worst distance from tier 0's drawn coastline: the discrete Hausdorff
    #: distance between the two simplified geometries, in degrees. Zero on tier 0.
    hausdorff_deg: float = 0.0
    #: The same distance measured in metres on the local equirectangular plane, which
    #: is what the runtime's thresholds are derived from. Zero on tier 0.
    hausdorff_m: float = 0.0
    #: The pair of points that attains `hausdorff_m`, as longitude and latitude.
    worst_pair: tuple[tuple[float, float], tuple[float, float]] = (
        (0.0, 0.0),
        (0.0, 0.0),
    )


@dataclass
class Land:
    """The four land tiers and the land the border mask tests against."""

    #: Coarsest last, in the order the asset stores them.
    tiers: list[LandTier]
    #: The drawn land of tier 0, which is what "on land" means to the borders.
    mask: BaseGeometry


@dataclass
class RoadChain:
    """One merged class-0 chain and the three resolutions the asset stores it at."""

    #: The chain's true length in metres, measured on its source geometry.
    length_m: float
    #: The latitude the chain's length and every fragment's length are measured at.
    reference_latitude: float
    #: The source geometry's segment count, for the build's distribution table.
    source_segments: int
    #: The geometry the fine copy stores, simplified at `ROAD_SIMPLIFY`.
    fine: list[tuple[float, float]]
    #: The geometry the middle copy stores, simplified at `MIDDLE_ROAD_SIMPLIFY`.
    middle: list[tuple[float, float]]
    #: The geometry the coarse copy stores, simplified at `COARSE_ROAD_SIMPLIFY`.
    coarse: list[tuple[float, float]]


class NationalRelations(osmium.SimpleHandler):
    """The national boundary relations of one extract and the ways they contain."""

    def __init__(self) -> None:
        super().__init__()
        self.relations: set[int] = set()
        self.member_ways: set[int] = set()

    def relation(self, relation: osmium.osm.Relation) -> None:
        tags = relation.tags
        if tags.get("boundary") != "administrative" or tags.get("admin_level") != "2":
            return
        self.relations.add(relation.id)
        for member in relation.members:
            if member.type == "w":
                self.member_ways.add(member.ref)


class Collector(osmium.SimpleHandler):
    """Collects the border and road ways of one extract."""

    def __init__(self, member_ways: set[int]) -> None:
        super().__init__()
        # The member ways of every national relation of the three extracts.
        self.member_ways = member_ways
        self.borders: dict[int, list[tuple[float, float]]] = {}
        # The packed border ways that carry no boundary tag of their own.
        self.relation_only: set[int] = set()
        self.roads: dict[int, tuple[str, list[tuple[float, float]]]] = {}
        self.admin_level_2 = 0
        self.members_seen = 0
        self.maritime_rejected = 0
        self.coastline_rejected = 0
        self.maritime_ids: set[int] = set()
        self.coastline_ids: set[int] = set()
        self.border_duplicates = 0
        self.road_duplicates = 0

    def way(self, handler: osmium.osm.Way) -> None:
        tags = handler.tags
        highway = tags.get("highway")
        if highway in ROAD_CLASSES:
            coords = way_coords(handler)
            if coords is None:
                return
            existing = self.roads.get(handler.id)
            if existing is not None:
                self.road_duplicates += 1
                if len(coords) <= len(existing[1]):
                    return
            self.roads[handler.id] = (highway, coords)
            return

        tagged = (
            tags.get("boundary") == "administrative" and tags.get("admin_level") == "2"
        )
        is_member = handler.id in self.member_ways
        if not tagged and not is_member:
            return
        if tagged:
            self.admin_level_2 += 1
        if is_member:
            self.members_seen += 1
        # The two tags a national boundary relation's member way may carry that make
        # it a border the runtime can never check: a maritime stretch and the coastline
        # the land already draws. Both are filtered here, for member ways as well as
        # for the ones selected by their own tags.
        if tags.get("maritime") == "yes":
            self.maritime_rejected += 1
            self.maritime_ids.add(handler.id)
            return
        if tags.get("natural") == "coastline":
            self.coastline_rejected += 1
            self.coastline_ids.add(handler.id)
            return
        coords = way_coords(handler)
        if coords is None:
            return
        existing = self.borders.get(handler.id)
        if existing is not None:
            self.border_duplicates += 1
            if len(coords) <= len(existing):
                return
        self.borders[handler.id] = coords
        if not tagged:
            self.relation_only.add(handler.id)


def way_coords(handler: osmium.osm.Way) -> list[tuple[float, float]] | None:
    """The way's node coordinates, or `None` when it has too few usable ones."""
    try:
        coords = [(node.location.lon, node.location.lat) for node in handler.nodes]
    except osmium.InvalidLocationError:
        return None
    if len(coords) < 2:
        return None
    return coords


def line_parts(geometry) -> list[LineString]:
    """Every `LineString` inside `geometry`, in order."""
    if geometry.is_empty:
        return []
    if isinstance(geometry, LineString):
        return [geometry]
    if hasattr(geometry, "geoms"):
        parts: list[LineString] = []
        for part in geometry.geoms:
            parts.extend(line_parts(part))
        return parts
    return []


def polygon_parts(geometry) -> list[Polygon]:
    """Every `Polygon` inside `geometry`, in order."""
    if geometry.is_empty:
        return []
    if isinstance(geometry, Polygon):
        return [geometry]
    if hasattr(geometry, "geoms"):
        parts: list[Polygon] = []
        for part in geometry.geoms:
            parts.extend(polygon_parts(part))
        return parts
    return []


def merge_lines(lines: list[LineString]) -> list[LineString]:
    """The lines joined into the longest chains their shared endpoints allow.

    The union nodes the network where two ways cross, and `linemerge` then walks every
    chain of degree-2 nodes into one line. The chains are sorted by their coordinates so
    the same input produces the same list in the same order.
    """
    chains = line_parts(linemerge(unary_union(lines)))
    chains.sort(
        key=lambda line: [coordinate for point in line.coords for coordinate in point]
    )
    return chains


def clean_coords(coords) -> list[tuple[float, float]]:
    """The piece's points exactly as stored: f32, consecutive duplicates dropped."""
    cleaned: list[tuple[float, float]] = []
    for point in coords:
        packed = (as_f32(point[0]), as_f32(point[1]))
        if not cleaned or packed != cleaned[-1]:
            cleaned.append(packed)
    return cleaned


def as_f32(value: float) -> float:
    """`value` rounded to the nearest `f32`, which is what the asset stores."""
    return struct.unpack("<f", struct.pack("<f", value))[0]


def reference_latitude(coords: list[tuple[float, float]]) -> float:
    """The mean latitude a polyline's length is measured at."""
    return sum(latitude for _, latitude in coords) / len(coords)


def length_metres(
    coords: list[tuple[float, float]], at_latitude: float
) -> float:
    """The polyline's length in metres, at one reference latitude.

    A degree of longitude is `cos(latitude)` times a degree of latitude, so the two
    axes are not interchangeable: the old formula used the longitude scale for both and
    underestimated a north-south road by 11%. Measuring every segment at the same
    reference latitude (the piece's own mean) also makes the length additive under a
    cell cut, which is what the chain-integrity check relies on.
    """
    cos_latitude = math.cos(math.radians(at_latitude))
    total = 0.0
    for (left_lon, left_lat), (right_lon, right_lat) in zip(coords, coords[1:]):
        total += math.hypot(
            (right_lon - left_lon) * cos_latitude,
            right_lat - left_lat,
        )
    return total * DEGREE_M


def unique_length_metres(
    coords: list[tuple[float, float]], at_latitude: float
) -> float:
    """The length of the polyline with a segment that is retraced counted once.

    A merged chain can go out and back along the same road; the cell cut's
    `intersection` deduplicates that segment, so the source length the integrity check
    compares against has to deduplicate it too. The 0.3 km gate uses the same figure:
    a road that retraces itself is not twice as long.
    """
    cos_latitude = math.cos(math.radians(at_latitude))
    seen: set[tuple[tuple[float, float], tuple[float, float]]] = set()
    total = 0.0
    for left, right in zip(coords, coords[1:]):
        key = (left, right) if left <= right else (right, left)
        if key in seen:
            continue
        seen.add(key)
        total += math.hypot(
            (right[0] - left[0]) * cos_latitude,
            right[1] - left[1],
        )
    return total * DEGREE_M


def segment_metres(
    left: tuple[float, float], right: tuple[float, float]
) -> float:
    """The length of one segment in metres, on the same local projection."""
    mid_latitude = math.radians((left[1] + right[1]) / 2.0)
    return (
        math.hypot(
            (right[0] - left[0]) * math.cos(mid_latitude),
            right[1] - left[1],
        )
        * DEGREE_M
    )


def read_pbfs(paths: list[Path]) -> Collector:
    """Read the national relations, then the border and road ways, of every extract.

    Borders are selected from the relations rather than from the ways' own tags: a
    national border is frequently mapped as untagged ways that are members of the
    country's relation, and selecting on `boundary` + `admin_level` alone leaves
    exactly those out. The relation pass is cheap (relations only, no node
    locations); the way pass is the one that has to resolve every node.
    """
    relations = NationalRelations()
    started = time.perf_counter()
    for path in paths:
        relations.apply_file(str(path))
    print(
        f"  {len(relations.relations):,} national relations, "
        f"{len(relations.member_ways):,} distinct member ways "
        f"({time.perf_counter() - started:.1f} s)",
        flush=True,
    )

    collector = Collector(relations.member_ways)
    for path in paths:
        started = time.perf_counter()
        before_borders = len(collector.borders)
        before_roads = len(collector.roads)
        before_admin = collector.admin_level_2
        before_members = collector.members_seen
        before_maritime = collector.maritime_rejected
        before_coastline = collector.coastline_rejected
        collector.apply_file(str(path), locations=True, idx="flex_mem")
        print(
            f"  {path.name}: "
            f"{collector.members_seen - before_members:,} national relation members "
            f"({collector.admin_level_2 - before_admin:,} of them tagged admin_level=2), "
            f"{collector.maritime_rejected - before_maritime:,} maritime and "
            f"{collector.coastline_rejected - before_coastline:,} coastline rejected, "
            f"+{len(collector.roads) - before_roads:,} road ways, "
            f"+{len(collector.borders) - before_borders:,} border ways "
            f"({time.perf_counter() - started:.1f} s)",
            flush=True,
        )
    print(
        f"  {len(relations.relations):,} national relations, "
        f"{len(relations.member_ways):,} distinct member ways; "
        f"{len(collector.borders):,} border ways kept, "
        f"{len(collector.relation_only):,} of them selected only by their relation; "
        f"{collector.maritime_rejected:,} maritime=yes and "
        f"{collector.coastline_rejected:,} natural=coastline rejected; "
        f"duplicate ways merged across extracts: {collector.border_duplicates:,} "
        f"border, {collector.road_duplicates:,} road",
        flush=True,
    )
    # The two border tags the runtime cannot check are asserted here: such a way must
    # never reach the packed border set, and the extracts must contain some, or the
    # filter is never exercised. The second count proves the filter runs over the
    # member ways a relation contributes, not only over the ways tagged in their own
    # right.
    rejected = collector.maritime_ids | collector.coastline_ids
    leaked = rejected.intersection(collector.borders)
    if leaked:
        raise SystemExit(
            f"error: {len(leaked):,} ways a national border excludes were packed as "
            "borders"
        )
    if not rejected:
        raise SystemExit(
            "error: no way selected as a border — tagged boundary=administrative + "
            "admin_level=2, or a member of such a relation — is tagged maritime=yes or "
            "natural=coastline, so the border filter was never exercised"
        )
    if not collector.relation_only:
        raise SystemExit(
            "error: no member way of a national relation without a boundary tag was "
            "selected, so selecting borders from the relations selected nothing"
        )
    return collector


def load_land(zip_path: Path, clip_box) -> Land:
    """The four land tiers, and the drawn land the border mask tests against.

    The map box is read, unioned, clipped and made valid **once**; every tier is
    then a `simplify` of that same geometry, so the four are one coastline at
    four resolutions. The mask is the tier 0 land, which is the geometry the map
    drew before the tiers existed: deriving it from a coarser tier would move the
    drawn coast by up to a kilometre and drag the borders with it.
    """
    with zipfile.ZipFile(zip_path) as archive:
        names = archive.namelist()
        shp_name = find_entry(names, ".shp")
        shx_name = find_entry(names, ".shx")
        started = time.perf_counter()
        polygons = []
        shapes_seen = 0
        with archive.open(shp_name) as shp_file, archive.open(shx_name) as shx_file:
            reader = shapefile.Reader(shp=shp_file, shx=shx_file)
            # The .dbf holds names and codes only; the geometry does not need it,
            # and opening it would decompress another file for nothing.
            for record in reader.iterShapes(bbox=(WEST, SOUTH, EAST, NORTH)):
                if record is None:
                    continue
                shapes_seen += 1
                geometry = shape(record.__geo_interface__)
                if not geometry.is_empty:
                    polygons.append(geometry)
    print(
        f"  {shapes_seen:,} shapes intersect the map box "
        f"({time.perf_counter() - started:.1f} s)",
        flush=True,
    )

    started = time.perf_counter()
    land = unary_union(polygons)
    clipped = land.intersection(clip_box)
    clipped = make_valid(clipped)
    print(
        f"  union + clip: {time.perf_counter() - started:.1f} s",
        flush=True,
    )

    tiers: list[LandTier] = []
    for tolerance in LAND_TIERS:
        started = time.perf_counter()
        simplified = clipped.simplify(tolerance, preserve_topology=True)
        rings: list[Piece] = []
        dropped = 0
        kept = 0
        largest_dropped_km2 = 0.0
        largest_dropped_at = (0.0, 0.0)
        for polygon in polygon_parts(simplified):
            area_km2 = (
                polygon.area
                * DEGREE_KM
                * DEGREE_KM
                * math.cos(math.radians(polygon.centroid.y))
            )
            if area_km2 < LAND_MIN_AREA_KM2:
                dropped += 1
                if area_km2 > largest_dropped_km2:
                    largest_dropped_km2 = area_km2
                    largest_dropped_at = (polygon.centroid.x, polygon.centroid.y)
                continue
            kept += 1
            for ring in [polygon.exterior, *polygon.interiors]:
                coords = clean_coords(ring.coords)
                if len(coords) < 4:
                    continue
                if coords[0] != coords[-1]:
                    coords.append(coords[0])
                rings.append(Piece(coords))
        print(
            f"  {tolerance}: {kept:,} polygons kept, {dropped:,} dropped under "
            f"{LAND_MIN_AREA_KM2} km2 (largest {largest_dropped_km2:.4f} km2 at "
            f"{largest_dropped_at[0]:.5f}E,{largest_dropped_at[1]:.5f}N); "
            f"{len(rings):,} rings, {sum(len(ring.coords) for ring in rings):,} points "
            f"({time.perf_counter() - started:.1f} s)",
            flush=True,
        )
        tiers.append(
            LandTier(
                tolerance,
                simplified,
                kept,
                dropped,
                largest_dropped_km2,
                largest_dropped_at,
                rings,
            )
        )

    # How far each coarser tier moved the coast: the discrete Hausdorff distance to
    # tier 0's drawn boundary, measured in metres on the same local equirectangular
    # plane `length_metres` uses. The degree figure is kept for reference; the metres
    # are what the runtime's thresholds are derived from (`src/model.rs`), and the
    # point pair is printed so the number can be checked by hand.
    tier0_local = local_metres(tiers[0].geometry.boundary)
    for tier in tiers[1:]:
        tier.hausdorff_deg = shapely.hausdorff_distance(
            tiers[0].geometry, tier.geometry
        )
        tier_local = local_metres(tier.geometry.boundary)
        metres, first, second = worst_local_pair(tier0_local, tier_local)
        check = shapely.hausdorff_distance(tier0_local, tier_local)
        if abs(check - metres) > 1e-6:
            raise SystemExit(
                f"error: the {tier.tolerance}° tier's worst pair is {metres:.6f} m, "
                f"but the boundary Hausdorff is {check:.6f} m"
            )
        tier.hausdorff_m = metres
        tier.worst_pair = (
            local_pair_to_degrees(first),
            local_pair_to_degrees(second),
        )
        (first_lon, first_lat), (second_lon, second_lat) = tier.worst_pair
        threshold = math.ceil(metres / TIER_PIXEL_BUDGET)
        print(
            f"  worst deviation from tier 0 at {tier.tolerance}: "
            f"{tier.hausdorff_deg:.6f} deg = {metres:.1f} m on the local metric = "
            f"{metres / FITTED_METRES_PER_PIXEL:.3f} px at the fitted zoom; "
            f"threshold {threshold} m/px; pair "
            f"({first_lon:.7f},{first_lat:.7f}) -> "
            f"({second_lon:.7f},{second_lat:.7f})",
            flush=True,
        )

    # The places a coarser tier must not erase, checked by point-in-polygon against
    # each tier's kept polygons (a point whose own polygon fell under the area
    # filter would be gone from the map).
    for tier in tiers:
        kept_polygons = [
            polygon
            for polygon in polygon_parts(tier.geometry)
            if polygon.area
            * DEGREE_KM
            * DEGREE_KM
            * math.cos(math.radians(polygon.centroid.y))
            >= LAND_MIN_AREA_KM2
        ]
        missing = [
            name
            for name, (longitude, latitude) in NAMED_PLACES.items()
            if not any(
                polygon.contains(Point(longitude, latitude))
                for polygon in kept_polygons
            )
        ]
        print(
            f"  named places kept at {tier.tolerance}: "
            f"{'all' if not missing else 'missing ' + ', '.join(missing)}",
            flush=True,
        )

    return Land(tiers, tiers[0].geometry)


def local_metres(geometry: BaseGeometry) -> BaseGeometry:
    """The geometry projected to metres on the local equirectangular plane.

    The same metric `length_metres` uses: a degree of longitude is `cos(latitude)`
    of a degree of latitude, applied at the map's scale latitude. A Douglas–Peucker
    tolerance is isotropic in degrees, so a north-south displacement is not shrunk
    by the longitude axis; this projection is what makes the Hausdorff distance a
    ground distance rather than a longitude-scaled one.
    """
    cos_latitude = math.cos(math.radians(SCALE_LATITUDE))
    return shapely.transform(
        geometry,
        lambda coordinates: coordinates * [DEGREE_M * cos_latitude, DEGREE_M],
    )


def worst_local_pair(
    reference: BaseGeometry, other: BaseGeometry
) -> tuple[float, tuple[float, float], tuple[float, float]]:
    """The worst boundary-to-boundary distance in metres, and the pair that attains it.

    Both geometries are the **boundaries** of two tiers, already projected by
    `local_metres`: the coast is a line, and a point inside the filled polygon is not
    a distance from the coast. The distance is the discrete Hausdorff distance the
    build has always measured — the largest distance from a vertex of one boundary
    to the other boundary — but in the local metric, so a north-south displacement
    is not multiplied by the longitude scale.
    """

    def farthest(
        source: BaseGeometry, target: BaseGeometry
    ) -> tuple[float, tuple[float, float], tuple[float, float]]:
        coordinates = shapely.get_coordinates(source)
        points = shapely.points(coordinates)
        distances = shapely.distance(target, points)
        index = int(max(range(len(distances)), key=lambda position: distances[position]))
        nearest = nearest_points(target, points[index])[0]
        return (
            float(distances[index]),
            (float(coordinates[index][0]), float(coordinates[index][1])),
            (float(nearest.x), float(nearest.y)),
        )

    forward = farthest(reference, other)
    backward = farthest(other, reference)
    return max(forward, backward, key=lambda entry: entry[0])


def local_pair_to_degrees(
    pair: tuple[float, float],
) -> tuple[float, float]:
    """The longitude and latitude a local-metre pair came from."""
    cos_latitude = math.cos(math.radians(SCALE_LATITUDE))
    return (pair[0] / (DEGREE_M * cos_latitude), pair[1] / DEGREE_M)


def way_touches_land(mask: BaseGeometry, coords: list[tuple[float, float]]) -> bool:
    """Whether any point of the way is on the land `mask`, or inside its margin.

    A prepared geometry is what makes this cheap: every candidate way is put through
    the same test against the same land.
    """
    return bool(
        shapely.contains_xy(
            mask,
            [point[0] for point in coords],
            [point[1] for point in coords],
        ).any()
    )


def find_entry(names: list[str], suffix: str) -> str:
    """The one archive entry ending in `suffix` (case-insensitive)."""
    matches = [name for name in names if name.lower().endswith(suffix)]
    if len(matches) != 1:
        raise SystemExit(
            f"error: expected exactly one {suffix} entry in the land zip, "
            f"found {matches}"
        )
    return matches[0]


def cell_edges(low: float, high: float, count: int) -> list[float]:
    """`count + 1` cell edges from `low` to `high`, exact at both ends."""
    edges = [low + (high - low) * index / count for index in range(count + 1)]
    edges[0] = low
    edges[-1] = high
    return edges


def cut_per_cell(
    pieces: list[Piece],
    label: str,
    measure_lengths: bool,
) -> tuple[list[Piece], list[tuple[int, int]]]:
    """Cut every piece at the grid lines so each piece lies in exactly one cell.

    `measure_lengths` recomputes `length_m` on the cut piece, which only the
    roads section uses; land and borders write zero. Returns the pieces in
    row-major cell order and the `(start, count)` table.
    """
    started = time.perf_counter()
    xs = cell_edges(WEST, EAST, GRID_COLS)
    ys = cell_edges(SOUTH, NORTH, GRID_ROWS)
    geometries = [LineString(piece.coords) for piece in pieces]
    tree = STRtree(geometries)

    ordered: list[Piece] = []
    cells: list[tuple[int, int]] = []
    for row in range(GRID_ROWS):
        for col in range(GRID_COLS):
            cell_box = box(xs[col], ys[row], xs[col + 1], ys[row + 1])
            found: list[Piece] = []
            for index in tree.query(cell_box):
                source = pieces[int(index)]
                for part in line_parts(geometries[int(index)].intersection(cell_box)):
                    coords = clean_coords(part.coords)
                    if len(coords) < 2:
                        continue
                    found.append(
                        Piece(
                            coords,
                            source.flags,
                            round(
                                length_metres(coords, source.reference_latitude)
                            )
                            if measure_lengths
                            else 0,
                            source.reference_latitude,
                            source.source,
                        )
                    )
            # Deterministic order inside the cell, independent of STRtree order.
            found.sort(key=piece_key)
            cells.append((len(ordered), len(found)))
            ordered.extend(found)
    print(
        f"  {label}: {len(ordered):,} pieces after cell clipping "
        f"({time.perf_counter() - started:.1f} s)",
        flush=True,
    )
    return ordered, cells


def assert_chain_partition(
    pieces: list[Piece],
    chains: list[tuple[list[tuple[float, float]], float]],
    label: str,
) -> None:
    """Checks that the cut pieces are exactly the chains, joined end to end.

    A cell cut may node a self-intersecting simplified chain, so a cut endpoint is
    allowed when it is a source chain endpoint, a cell edge (the known f32 bbox sliver
    at the map border), or a point on a source chain. The real proof is the length:
    every piece is measured at the same reference latitude, so a chain's fragments sum
    to its own length and a missing fragment shows up as a short sum.
    """
    chain_lines = [LineString(coords) for coords, _ in chains]
    chain_tree = STRtree(chain_lines)
    chain_ends: set[tuple[float, float]] = set()
    source_lengths: list[float] = []
    for coords, at_latitude in chains:
        chain_ends.add(coords[0])
        chain_ends.add(coords[-1])
        source_lengths.append(unique_length_metres(coords, at_latitude))
    counts: Counter[tuple[float, float]] = Counter()
    cut_lengths = [0.0] * len(chains)
    for piece in pieces:
        if not 0 <= piece.source < len(chains):
            raise SystemExit(f"error: a {label} piece has no source chain")
        counts[piece.coords[0]] += 1
        counts[piece.coords[-1]] += 1
        cut_lengths[piece.source] += length_metres(
            piece.coords, piece.reference_latitude
        )
    edge_tolerance = 2e-6
    xs = cell_edges(WEST, EAST, GRID_COLS)
    ys = cell_edges(SOUTH, NORTH, GRID_ROWS)

    def on_cell_edge(point: tuple[float, float]) -> bool:
        return min(abs(point[0] - x) for x in xs) <= edge_tolerance or min(
            abs(point[1] - y) for y in ys
        ) <= edge_tolerance

    def on_chain(point: tuple[float, float]) -> bool:
        nearest = int(chain_tree.nearest(Point(point)))
        return chain_lines[nearest].distance(Point(point)) <= 1e-6

    gaps = [
        point
        for point, count in counts.items()
        if point not in chain_ends
        and not on_cell_edge(point)
        and not on_chain(point)
        and count % 2 != 0
    ]
    if gaps:
        raise SystemExit(
            f"error: {len(gaps):,} {label} cut endpoints are neither a source chain "
            "endpoint, a cell edge, a point on a chain, nor shared by two pieces"
        )
    # The fixed reference latitude makes the fragment lengths additive. The metre of
    # slack is the f32 rounding of the cut points; a missing fragment is much longer.
    for index, (source_length, cut_length) in enumerate(
        zip(source_lengths, cut_lengths, strict=True)
    ):
        if abs(cut_length - source_length) > 1.0:
            raise SystemExit(
                f"error: {label} chain {index} measures {cut_length:,.1f} m, its "
                f"source {source_length:,.1f} m"
            )


def piece_key(piece: Piece) -> list[float]:
    """A total order over pieces: their coordinates, then their flags."""
    key = [coordinate for point in piece.coords for coordinate in point]
    key.append(float(piece.flags))
    return key


def pack_section(
    kind: int,
    pieces: list[Piece],
    cells: list[tuple[int, int]],
    min_points: int,
) -> bytes:
    """One KHL2 section: header, points, 12-byte piece headers, cell table."""
    points: list[tuple[float, float]] = []
    headers: list[tuple[int, int, int, int]] = []
    for piece in pieces:
        if not min_points <= len(piece.coords) <= 0xFFFF:
            raise SystemExit(
                f"error: a piece of kind {kind} has {len(piece.coords)} points, "
                f"which is outside {min_points}..65535"
            )
        headers.append((len(points), len(piece.coords), piece.flags, piece.length_m))
        points.extend(piece.coords)

    out = bytearray()
    out += struct.pack("<IIII", kind, len(points), len(pieces), len(cells))
    for longitude, latitude in points:
        out += struct.pack("<ff", longitude, latitude)
    for start, length, flags, length_m in headers:
        out += struct.pack("<IHBBI", start, length, flags, 0, length_m)
    for start, count in cells:
        out += struct.pack("<II", start, count)
    return bytes(out)


def lod_pieces(pieces: list[Piece]) -> dict[str, list[Piece]]:
    """The pieces each runtime road LOD keeps."""
    return {
        "COARSE": [piece for piece in pieces if piece.flags & FLAG_COARSE],
        "MIDDLE": [piece for piece in pieces if piece.flags & FLAG_MIDDLE],
        "FINE": [
            piece
            for piece in pieces
            if not piece.flags & (FLAG_COARSE | FLAG_MIDDLE | FLAG_PRIMARY)
        ],
        "DE": [
            piece
            for piece in pieces
            if not piece.flags & (FLAG_COARSE | FLAG_MIDDLE)
        ],
    }


def dangling_endpoints(pieces: list[Piece]) -> tuple[int, int]:
    """Endpoints of the selected pieces no other selected piece shares."""
    ends: Counter[tuple[float, float]] = Counter()
    for piece in pieces:
        ends[piece.coords[0]] += 1
        ends[piece.coords[-1]] += 1
    dangling = sum(1 for count in ends.values() if count == 1)
    pieces_with_dangling = sum(
        1
        for piece in pieces
        if ends[piece.coords[0]] == 1 or ends[piece.coords[-1]] == 1
    )
    return dangling, pieces_with_dangling


def short_segments(pieces: list[Piece], limit_m: float = 16.0) -> int:
    """Segments of the pieces shorter than `limit_m`."""
    count = 0
    for piece in pieces:
        for left, right in zip(piece.coords, piece.coords[1:]):
            if segment_metres(left, right) < limit_m:
                count += 1
    return count


def build(assets: Path, out: Path) -> None:
    """Build `out` from the four sources in `assets`."""
    started = time.perf_counter()
    clip_box = box(WEST, SOUTH, EAST, NORTH)

    print("reading the extracts", flush=True)
    collector = read_pbfs(
        [
            assets / "gcc-states-latest.osm.pbf",
            assets / "iran-latest.osm.pbf",
            assets / "pakistan-latest.osm.pbf",
        ]
    )

    print("land", flush=True)
    land = load_land(assets / "land-polygons-split-4326.zip", clip_box)
    for tier in land.tiers:
        tier.rings.sort(key=piece_key)
    longitudes = [
        point[0] for ring in land.tiers[0].rings for point in ring.coords
    ]
    latitudes = [point[1] for ring in land.tiers[0].rings for point in ring.coords]
    print(
        f"  extent {min(longitudes):.6f} {min(latitudes):.6f} "
        f"{max(longitudes):.6f} {max(latitudes):.6f}",
        flush=True,
    )
    # The border land mask: the drawn land grown by the margin a border point gets, prepared
    # once for the point test every candidate way is put through.
    land_mask = land.mask.buffer(BORDER_LAND_MASK_TOLERANCE)
    shapely.prepare(land_mask)

    print("borders", flush=True)
    border_pieces: list[Piece] = []
    relation_only_in_box = 0
    sea_only: list[int] = []
    for way_id, coords in collector.borders.items():
        line = LineString(coords)
        if not line.intersects(clip_box):
            continue
        # A member way of a national relation can be a sea boundary that carries no
        # `maritime=yes` tag, which the tag filter cannot see. A way with a single point on
        # land is a border that also crosses water — the two Saudi–Bahrain ways at the coast
        # are exactly that — so the test is "none of its points", never a share of them.
        if not way_touches_land(land_mask, coords):
            sea_only.append(way_id)
            continue
        if way_id in collector.relation_only:
            relation_only_in_box += 1
        for part in line_parts(line.intersection(clip_box)):
            simplified = clean_coords(
                part.simplify(BORDER_SIMPLIFY, preserve_topology=False).coords
            )
            if len(simplified) >= 2:
                border_pieces.append(Piece(simplified))
    print(
        f"  {len(border_pieces):,} pieces, "
        f"{sum(len(p.coords) for p in border_pieces):,} points; "
        f"{relation_only_in_box:,} of the {len(collector.relation_only):,} ways "
        "selected only by their relation reach the box; "
        f"{len(sea_only):,} ways dropped with no point on land "
        f"({', '.join(str(way_id) for way_id in sorted(sea_only))})",
        flush=True,
    )
    if relation_only_in_box == 0:
        raise SystemExit(
            "error: no border way selected only by its national relation reaches the "
            "map box, so the Saudi–UAE land border is still missing"
        )
    if not sea_only:
        raise SystemExit(
            "error: no border way lies entirely over water, so the land mask was never "
            "exercised"
        )

    print("roads", flush=True)
    primary_parts: list[LineString] = []
    class_zero: list[LineString] = []
    for highway, coords in collector.roads.values():
        line = LineString(coords)
        if not line.intersects(clip_box):
            continue
        for part in line_parts(line.intersection(clip_box)):
            if highway == "primary":
                primary_parts.append(part)
                continue
            class_zero.append(part)

    # All three copies of class 0 are the network joined into the chains its shared
    # endpoints allow. A per-way simplification cannot thin a two-node way, and most of
    # the network's ways are exactly that, so joining them first is what makes the
    # coarser copies coarse at all. Building the fine copy from the same chains is what
    # lets the short ones be dropped as whole roads: the copies then carry the same
    # chain identity, and a chain is either entirely drawn or entirely absent at a
    # given scale.
    coarse_chains = merge_lines(class_zero)
    chains: list[RoadChain] = []
    for chain in coarse_chains:
        source = clean_coords(chain.coords)
        reference = reference_latitude(source)
        fine = clean_coords(
            chain.simplify(ROAD_SIMPLIFY, preserve_topology=False).coords
        )
        middle = clean_coords(
            chain.simplify(MIDDLE_ROAD_SIMPLIFY, preserve_topology=False).coords
        )
        coarse = clean_coords(
            chain.simplify(COARSE_ROAD_SIMPLIFY, preserve_topology=False).coords
        )
        # A chain shorter than the tolerance can simplify to a single point. Keep its
        # own geometry rather than dropping it: a chain that is drawn must be complete.
        if len(fine) < 2:
            fine = source
        if len(middle) < 2:
            middle = source
        if len(coarse) < 2:
            coarse = source
        chains.append(
            RoadChain(
                unique_length_metres(source, reference),
                reference,
                len(source) - 1,
                fine,
                middle,
                coarse,
            )
        )
    if any(
        len(chain.fine) < 2 or len(chain.middle) < 2 or len(chain.coarse) < 2
        for chain in chains
    ):
        raise SystemExit("error: a class-0 chain has no geometry")

    # The chain-length distribution is what the low-zoom feature dropping is chosen
    # from: a chain is a whole road between junctions, so dropping a short one leaves
    # the drawn network continuous while a short *piece* of a long chain would not.
    print("  class-0 chains by length", flush=True)
    for low, high in [(0, 1), (1, 2), (2, 5), (5, 10), (10, 20), (20, None)]:
        selected = [
            chain
            for chain in chains
            if chain.length_m >= low * 1_000
            and (high is None or chain.length_m < high * 1_000)
        ]
        source_segments = sum(chain.source_segments for chain in selected)
        coarse_segments = sum(len(chain.coarse) - 1 for chain in selected)
        label = f"{low}-{high} km" if high is not None else f"{low}+ km"
        print(
            f"    {label:9s}: {len(selected):6,d} chains, "
            f"{source_segments:7,d} source segments, "
            f"{coarse_segments:7,d} coarse segments, "
            f"{2 * coarse_segments:7,d} triangles",
            flush=True,
        )
    print(
        "  coarse copy with chains shorter than L dropped "
        "(segments are before the per-cell cut)",
        flush=True,
    )
    for limit_km in [0.25, 0.3, 0.5, 1, 1.5, 2, 3, 4, 6, 10]:
        kept = [
            chain for chain in chains if chain.length_m >= limit_km * 1_000
        ]
        segments = sum(len(chain.coarse) - 1 for chain in kept)
        print(
            f"    < {limit_km:5.2f} km: "
            f"{len(chains) - len(kept):6,d} chains dropped, "
            f"{segments:7,d} kept segments, {2 * segments:7,d} kept triangles",
            flush=True,
        )

    short = [chain for chain in chains if chain.length_m < SHORT_CHAIN_KM * 1_000]
    kept = [chain for chain in chains if chain.length_m >= SHORT_CHAIN_KM * 1_000]
    if not kept:
        raise SystemExit(
            f"error: every class-0 chain is shorter than {SHORT_CHAIN_KM} km, so the "
            "coarse copy would be empty"
        )
    fine_class_zero = [
        Piece(
            chain.fine,
            FLAG_SHORT_CHAIN if chain.length_m < SHORT_CHAIN_KM * 1_000 else 0,
            0,
            chain.reference_latitude,
            index,
        )
        for index, chain in enumerate(chains)
    ]
    middle_pieces = [
        Piece(chain.middle, FLAG_MIDDLE, 0, chain.reference_latitude, index)
        for index, chain in enumerate(kept)
    ]
    coarse_pieces = [
        Piece(chain.coarse, FLAG_COARSE, 0, chain.reference_latitude, index)
        for index, chain in enumerate(kept)
    ]

    # The primary network gets the same chain treatment and the same 0.3 km feature
    # dropping. It is drawn only at the closest zoom, where 0.3 km is 1.2 px, so there
    # is no coarser primary scale at which a dropped chain would come back: the drop is
    # build-time, not a runtime gate.
    primary_chains = merge_lines(primary_parts)
    primary_geometry: list[tuple[float, float, list[tuple[float, float]]]] = []
    for chain in primary_chains:
        source = clean_coords(chain.coords)
        reference = reference_latitude(source)
        simplified = clean_coords(
            chain.simplify(ROAD_SIMPLIFY, preserve_topology=False).coords
        )
        if len(simplified) < 2:
            simplified = source
        primary_geometry.append(
            (unique_length_metres(source, reference), reference, simplified)
        )
    primary_kept = [
        entry for entry in primary_geometry if entry[0] >= SHORT_CHAIN_KM * 1_000
    ]
    if not primary_kept:
        raise SystemExit(
            f"error: every primary chain is shorter than {SHORT_CHAIN_KM} km"
        )
    primary_pieces = [
        Piece(coords, FLAG_PRIMARY, 0, reference, index)
        for index, (_, reference, coords) in enumerate(primary_kept)
    ]
    primary_points = sum(len(piece.coords) for piece in primary_pieces)
    print(
        f"  primary {len(primary_chains):,} chains, "
        f"{len(primary_kept):,} kept (>= {SHORT_CHAIN_KM} km), "
        f"{len(primary_chains) - len(primary_kept):,} short dropped; "
        f"{len(primary_pieces):,} chains, {primary_points:,} points, "
        f"{2 * (primary_points - len(primary_pieces)):,} triangles",
        flush=True,
    )

    road_pieces = [*primary_pieces, *fine_class_zero, *middle_pieces, *coarse_pieces]
    fine_points = sum(len(piece.coords) for piece in fine_class_zero)
    middle_points = sum(len(piece.coords) for piece in middle_pieces)
    coarse_points = sum(len(piece.coords) for piece in coarse_pieces)
    print(
        f"  class-0 fine {len(fine_class_zero):,} chains, {fine_points:,} points, "
        f"{2 * (fine_points - len(fine_class_zero)):,} triangles "
        f"({len(short):,} short chains flagged); "
        f"middle {len(middle_pieces):,} chains, {middle_points:,} points, "
        f"{2 * (middle_points - len(middle_pieces)):,} triangles; "
        f"coarse {len(coarse_pieces):,} chains, {coarse_points:,} points, "
        f"{2 * (coarse_points - len(coarse_pieces)):,} triangles "
        f"({len(short):,} short chains dropped); "
        f"primary {len(primary_pieces):,} chains, {primary_points:,} points",
        flush=True,
    )

    print("cutting borders and roads per cell", flush=True)
    border_pieces, border_cells = cut_per_cell(
        border_pieces, "borders", measure_lengths=False
    )
    road_pieces, road_cells = cut_per_cell(road_pieces, "roads", measure_lengths=True)
    # Every copy's cell cut has to rejoin to its own chain geometry, not a sample of
    # pieces. The fine copy is the only one that holds the short chains, so it proves
    # the whole network; the middle and coarse copies prove the long chains they store,
    # and the primary copy proves its own chains.
    assert_chain_partition(
        [
            piece
            for piece in road_pieces
            if piece.flags & (FLAG_COARSE | FLAG_MIDDLE | FLAG_PRIMARY) == 0
        ],
        [(chain.fine, chain.reference_latitude) for chain in chains],
        "fine",
    )
    assert_chain_partition(
        [piece for piece in road_pieces if piece.flags & FLAG_MIDDLE],
        [(chain.middle, chain.reference_latitude) for chain in kept],
        "middle",
    )
    assert_chain_partition(
        [piece for piece in road_pieces if piece.flags & FLAG_COARSE],
        [(chain.coarse, chain.reference_latitude) for chain in kept],
        "coarse",
    )
    assert_chain_partition(
        [piece for piece in road_pieces if piece.flags & FLAG_PRIMARY],
        [(coords, reference) for _, reference, coords in primary_kept],
        "primary",
    )

    print("packing", flush=True)
    blob = bytearray()
    blob += b"KHL2"
    blob += struct.pack("<I", VERSION)
    blob += struct.pack("<4f", WEST, SOUTH, EAST, NORTH)
    blob += struct.pack("<III", GRID_COLS, GRID_ROWS, SECTION_COUNT)
    # Every land tier stores rings rather than a cell table: the parser tessellates
    # them and bins the triangles by centroid at run time.
    sections = [
        (kind, tier.rings, [], 3)
        for kind, tier in zip(LAND_KINDS, land.tiers, strict=True)
    ]
    sections.append((KIND_BORDERS, border_pieces, border_cells, 2))
    sections.append((KIND_ROADS, road_pieces, road_cells, 2))
    for kind, pieces, cells, min_points in sections:
        blob += pack_section(kind, pieces, cells, min_points)
    blob = bytes(blob)

    print("")
    print("sections")
    for kind, pieces, cells, _ in sections:
        point_count = sum(len(piece.coords) for piece in pieces)
        print(
            f"  kind {kind}  points {point_count:8,d}  pieces {len(pieces):7,d}  "
            f"cells {len(cells):6,d}"
        )
    print(f"file: {len(blob):,} bytes ({len(blob) / (1024 * 1024):.2f} MiB)")
    if len(blob) > MAX_ASSET_BYTES:
        raise SystemExit(
            f"error: the asset is {len(blob):,} bytes, over the "
            f"{MAX_ASSET_BYTES:,}-byte budget"
        )

    print("")
    print("land tiers (triangles estimated as points - 3 x rings)")
    for kind, tier in zip(LAND_KINDS, land.tiers, strict=True):
        points = sum(len(ring.coords) for ring in tier.rings)
        print(
            f"  kind {kind}  tier {tier.tolerance}°  rings {len(tier.rings):6,d}  "
            f"points {points:8,d}  triangles {points - 3 * len(tier.rings):9,d}"
        )

    print("")
    print("road LODs (triangles = 2 x (vertices - pieces))")
    lods = lod_pieces(road_pieces)
    for name, selected in lods.items():
        vertices = sum(len(piece.coords) for piece in selected)
        print(
            f"  {name:6s}  pieces {len(selected):7,d}  vertices {vertices:8,d}  "
            f"triangles {2 * (vertices - len(selected)):9,d}"
        )
    print(
        f"  class-0 pieces: fine all {len(lods['FINE']):,}, middle kept "
        f"{len(lods['MIDDLE']):,}, coarse kept {len(lods['COARSE']):,} "
        "(the middle and coarse copies are subsets of the fine one)"
    )
    # The coarse copy is the long chains at the fitted zoom; the short ones are
    # feature-dropped and come back through the fine copy. The dangling endpoints are
    # the junctions whose short connector is gone, the build's own measure of that
    # visual cost.
    dangling, pieces_with_dangling = dangling_endpoints(lods["COARSE"])
    print(
        f"  COARSE dangling endpoints: {dangling:,} "
        f"(on {pieces_with_dangling:,} pieces)"
    )
    print(
        f"segments shorter than 16 m: "
        f"{', '.join(f'land tier {index} {short_segments(tier.rings):,}' for index, tier in enumerate(land.tiers))}, "
        f"borders {short_segments(border_pieces):,}, "
        f"roads {short_segments(lods['FINE']):,}, "
        f"middle {short_segments(lods['MIDDLE']):,}, "
        f"coarse {short_segments(lods['COARSE']):,}"
    )

    temporary = out.with_name(out.name + ".tmp")
    # A `.tmp` from an interrupted earlier build is never reused: the rename is
    # what publishes the asset, so start from a clean file and drop it again if
    # this write does not finish.
    temporary.unlink(missing_ok=True)
    try:
        temporary.write_bytes(blob)
        temporary.replace(out)
    except BaseException:
        temporary.unlink(missing_ok=True)
        raise
    print(f"\nwrote {out}")
    print(f"finished in {time.perf_counter() - started:.1f} s")


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Pack the OSM sources into the KHL2 map asset.",
    )
    parser.add_argument(
        "--assets",
        default=str(Path(__file__).resolve().parent),
        help="directory holding the four sources (default: this script's directory)",
    )
    parser.add_argument(
        "--out",
        default=None,
        help="the asset to write (default: <assets>/hormuz_map.bin)",
    )
    arguments = parser.parse_args()

    assets = Path(arguments.assets).resolve()
    out = Path(arguments.out).resolve() if arguments.out else assets / "hormuz_map.bin"

    missing = [source for source in download_map.SOURCES if not (assets / source.name).is_file()]
    if missing:
        for source in missing:
            print(f"missing {source.name}")
        print(f'run "{download_map.DOWNLOAD_COMMAND}" first')
        raise SystemExit(1)

    for source in download_map.SOURCES:
        size = (assets / source.name).stat().st_size
        floor = LAND_ZIP_MIN_BYTES if source.name.endswith(".zip") else PBF_MIN_BYTES
        if size < floor:
            print(
                f"error: {source.name} is only {size:,} bytes, below the "
                f"{floor:,}-byte floor for a complete download; run "
                f"`{download_map.DOWNLOAD_COMMAND} --force`",
                file=sys.stderr,
            )
            raise SystemExit(1)
        if source.snapshot_bytes and size != source.snapshot_bytes:
            print(
                f"warning: {source.name} is {size:,} bytes, the recorded snapshot "
                f"is {source.snapshot_bytes:,}; the upstream data may be newer",
                file=sys.stderr,
            )

    build(assets, out)


if __name__ == "__main__":
    main()
