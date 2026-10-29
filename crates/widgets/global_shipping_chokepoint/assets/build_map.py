"""Pack the OSM sources into `hormuz_map.bin`, the KHL2 map asset.

Run `download_map.py` first: this script reads the four sources next to itself
and fails with an actionable message when one is missing.

    python crates/widgets/global_shipping_chokepoint/assets/build_map.py

What is packed (all little-endian, see `README.md` for the format):

1. **land** — OSM land polygons, clipped to the map box, simplified to ~111 m,
   islands under 0.01 km² dropped, one closed ring per polygon ring.
2. **borders** — `boundary=administrative` + `admin_level=2` ways without
   `maritime=yes`, clipped and simplified to ~55 m, cut per grid cell.
3. **roads** — `highway=motorway|trunk|primary`, clipped and simplified to
   ~134 m, cut per grid cell, with the two flags the runtime LOD filter reads.

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
from shapely import STRtree, box, make_valid
from shapely.geometry import LineString, Polygon, shape
from shapely.ops import unary_union

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

#: Section kinds of the KHL2 asset.
KIND_LAND, KIND_BORDERS, KIND_ROADS = 1, 2, 3

#: Road flags: bit 0 is "primary", bit 1 is "the way carries a `ref`".
FLAG_PRIMARY = 1 << 0
FLAG_HAS_REF = 1 << 1

#: The road classes the asset keeps, as `highway` tag values.
ROAD_CLASSES = frozenset({"motorway", "trunk", "primary"})

#: Land is simplified to 0.001° (about 111 m) with its topology kept.
LAND_SIMPLIFY = 0.001
#: Islands smaller than this are dropped. The area is converted to km² first:
#: a raw square degree is about 123 km² here and would drop almost every island.
LAND_MIN_AREA_KM2 = 0.01
#: Borders are simplified to 0.0005° (about 55 m).
BORDER_SIMPLIFY = 0.0005
#: Roads are simplified to 0.0012° (about 134 m).
ROAD_SIMPLIFY = 0.0012

#: Degrees to kilometres at the equator; every area scales it by the cosine of
#: the latitude.
DEGREE_KM = 111.32
#: Degrees to metres at the equator; every length scales it by the cosine of the
#: latitude.
DEGREE_M = 111_320.0

#: The runtime E_MIN filter: `length_m >= 1000`.
EMIN_LENGTH_M = 1000

#: Below these sizes a source is not a plausible download of that file.
PBF_MIN_BYTES = 20 * 1024**2
LAND_ZIP_MIN_BYTES = 500 * 1024**2

#: The asset budget from the plan.
MAX_ASSET_BYTES = 2 * 1024 * 1024


@dataclass
class Piece:
    """One packed polyline: a border fragment, a road fragment or a land ring."""

    coords: list[tuple[float, float]]
    flags: int = 0
    length_m: int = 0


class Collector(osmium.SimpleHandler):
    """Collects the border and road ways of one extract."""

    def __init__(self) -> None:
        super().__init__()
        self.borders: dict[int, list[tuple[float, float]]] = {}
        self.roads: dict[int, tuple[str, bool, list[tuple[float, float]]]] = {}
        self.admin_level_2 = 0
        self.maritime_rejected = 0
        self.maritime_ids: set[int] = set()
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
                if len(coords) <= len(existing[2]):
                    return
            self.roads[handler.id] = (highway, "ref" in tags, coords)
            return

        if tags.get("boundary") != "administrative" or tags.get("admin_level") != "2":
            return
        self.admin_level_2 += 1
        if tags.get("maritime") == "yes":
            self.maritime_rejected += 1
            self.maritime_ids.add(handler.id)
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


def length_metres(coords: list[tuple[float, float]]) -> float:
    """The polyline's length in metres, scaled at its centroid latitude."""
    line = LineString(coords)
    return line.length * DEGREE_M * math.cos(math.radians(line.centroid.y))


def segment_metres(
    left: tuple[float, float], right: tuple[float, float]
) -> float:
    """The length of one segment in metres, scaled at its mid latitude."""
    latitude = (left[1] + right[1]) / 2.0
    return (
        math.hypot(right[0] - left[0], right[1] - left[1])
        * DEGREE_M
        * math.cos(math.radians(latitude))
    )


def read_pbfs(paths: list[Path]) -> Collector:
    """Read the border and road ways of every extract into one collector."""
    collector = Collector()
    for path in paths:
        started = time.perf_counter()
        before_borders = len(collector.borders)
        before_roads = len(collector.roads)
        before_admin = collector.admin_level_2
        before_maritime = collector.maritime_rejected
        collector.apply_file(str(path), locations=True, idx="flex_mem")
        print(
            f"  {path.name}: "
            f"{collector.admin_level_2 - before_admin:,} admin_level=2 ways "
            f"({collector.maritime_rejected - before_maritime:,} maritime rejected), "
            f"+{len(collector.roads) - before_roads:,} road ways, "
            f"+{len(collector.borders) - before_borders:,} border ways "
            f"({time.perf_counter() - started:.1f} s)",
            flush=True,
        )
    print(
        f"  {collector.admin_level_2:,} admin_level=2 ways, "
        f"{collector.maritime_rejected:,} maritime rejected; "
        f"duplicate ways merged across extracts: {collector.border_duplicates:,} "
        f"border, {collector.road_duplicates:,} road",
        flush=True,
    )
    # The maritime filter is the one border rule the runtime cannot check, so the build
    # asserts it here: a way tagged `maritime=yes` must never reach the packed border
    # set, and the extracts must contain some, or the filter is never exercised.
    leaked = collector.maritime_ids.intersection(collector.borders)
    if leaked:
        raise SystemExit(
            f"error: {len(leaked):,} ways tagged maritime=yes were packed as borders"
        )
    if not collector.maritime_ids:
        raise SystemExit(
            "error: no boundary=administrative + admin_level=2 way is tagged "
            "maritime=yes, so the maritime filter was never exercised"
        )
    return collector


def load_land(zip_path: Path, clip_box) -> list[Piece]:
    """The land rings: every polygon ring of the clipped, simplified land."""
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
    simplified = clipped.simplify(LAND_SIMPLIFY, preserve_topology=True)
    print(
        f"  union + clip + simplify: {time.perf_counter() - started:.1f} s",
        flush=True,
    )

    rings: list[Piece] = []
    dropped = 0
    kept = 0
    for polygon in polygon_parts(simplified):
        area_km2 = (
            polygon.area
            * DEGREE_KM
            * DEGREE_KM
            * math.cos(math.radians(polygon.centroid.y))
        )
        if area_km2 < LAND_MIN_AREA_KM2:
            dropped += 1
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
        f"  {kept:,} polygons kept, {dropped:,} dropped under "
        f"{LAND_MIN_AREA_KM2} km2; {len(rings):,} rings",
        flush=True,
    )
    return rings


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
                            round(length_metres(coords)) if measure_lengths else 0,
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


def piece_key(piece: Piece) -> list[float]:
    """A total order over pieces: their coordinates, then their flags."""
    key = [coordinate for point in piece.coords for coordinate in point]
    key.append(float(piece.flags))
    return key


def pack_section(
    kind: int,
    pieces: list[Piece],
    cells: list[tuple[int, int]],
) -> bytes:
    """One KHL2 section: header, points, 12-byte piece headers, cell table."""
    points: list[tuple[float, float]] = []
    headers: list[tuple[int, int, int, int]] = []
    for piece in pieces:
        if not 2 <= len(piece.coords) <= 0xFFFF:
            raise SystemExit(
                f"error: a piece of kind {kind} has {len(piece.coords)} points, "
                "which does not fit the u16 length"
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
        "E_MIN": [
            piece
            for piece in pieces
            if not piece.flags & FLAG_PRIMARY
            and piece.flags & FLAG_HAS_REF
            and piece.length_m >= EMIN_LENGTH_M
        ],
        "E_ALL": [piece for piece in pieces if not piece.flags & FLAG_PRIMARY],
        "DE": pieces,
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

    print("borders", flush=True)
    border_pieces: list[Piece] = []
    for coords in collector.borders.values():
        line = LineString(coords)
        if not line.intersects(clip_box):
            continue
        for part in line_parts(line.intersection(clip_box)):
            simplified = clean_coords(
                part.simplify(BORDER_SIMPLIFY, preserve_topology=False).coords
            )
            if len(simplified) >= 2:
                border_pieces.append(Piece(simplified))
    print(
        f"  {len(border_pieces):,} pieces, "
        f"{sum(len(p.coords) for p in border_pieces):,} points",
        flush=True,
    )

    print("roads", flush=True)
    road_pieces: list[Piece] = []
    for highway, has_ref, coords in collector.roads.values():
        line = LineString(coords)
        if not line.intersects(clip_box):
            continue
        for part in line_parts(line.intersection(clip_box)):
            part = part.simplify(ROAD_SIMPLIFY, preserve_topology=False)
            simplified = clean_coords(part.coords)
            if len(simplified) < 2:
                continue
            flags = (FLAG_PRIMARY if highway == "primary" else 0) | (
                FLAG_HAS_REF if has_ref else 0
            )
            road_pieces.append(Piece(simplified, flags, 0))
    print(
        f"  {len(road_pieces):,} pieces, "
        f"{sum(len(p.coords) for p in road_pieces):,} points",
        flush=True,
    )

    print("land", flush=True)
    land_rings = load_land(assets / "land-polygons-split-4326.zip", clip_box)
    land_rings.sort(key=piece_key)
    longitudes = [point[0] for ring in land_rings for point in ring.coords]
    latitudes = [point[1] for ring in land_rings for point in ring.coords]
    print(
        f"  extent {min(longitudes):.6f} {min(latitudes):.6f} "
        f"{max(longitudes):.6f} {max(latitudes):.6f}",
        flush=True,
    )

    print("cutting borders and roads per cell", flush=True)
    border_pieces, border_cells = cut_per_cell(
        border_pieces, "borders", measure_lengths=False
    )
    road_pieces, road_cells = cut_per_cell(road_pieces, "roads", measure_lengths=True)

    print("packing", flush=True)
    blob = bytearray()
    blob += b"KHL2"
    blob += struct.pack("<I", 1)
    blob += struct.pack("<4f", WEST, SOUTH, EAST, NORTH)
    blob += struct.pack("<III", GRID_COLS, GRID_ROWS, 3)
    sections = [
        (KIND_LAND, land_rings, []),
        (KIND_BORDERS, border_pieces, border_cells),
        (KIND_ROADS, road_pieces, road_cells),
    ]
    for kind, pieces, cells in sections:
        blob += pack_section(kind, pieces, cells)
    blob = bytes(blob)

    print("")
    print("sections")
    for kind, pieces, cells in sections:
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
    print("road LODs (triangles = 2 x (vertices - pieces))")
    lods = lod_pieces(road_pieces)
    for name, selected in lods.items():
        vertices = sum(len(piece.coords) for piece in selected)
        print(
            f"  {name:5s}  pieces {len(selected):7,d}  vertices {vertices:8,d}  "
            f"triangles {2 * (vertices - len(selected)):9,d}"
        )
    emin_pieces = lods["E_MIN"]
    dangling, pieces_with_dangling = dangling_endpoints(emin_pieces)
    print(f"  E_MIN filtered-out pieces: {len(road_pieces) - len(emin_pieces):,}")
    print(
        f"  E_MIN dangling endpoints: {dangling:,} "
        f"(on {pieces_with_dangling:,} pieces)"
    )
    print(
        f"segments shorter than 16 m: land {short_segments(land_rings):,}, "
        f"borders {short_segments(border_pieces):,}, "
        f"roads {short_segments(road_pieces):,}"
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
