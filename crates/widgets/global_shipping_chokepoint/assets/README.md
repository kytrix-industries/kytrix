# `hormuz_map.bin`

The OSM vector base map the Hormuz vessel widget draws under its vessels: the
coastline, the land borders and the motorway/trunk/primary roads around the
Strait of Hormuz, packed into one KHL2 file and embedded with `include_bytes!`.

`src/scene.rs` parses it as KHL2; it is the only asset here.

## Two steps: download, then build

```powershell
python crates/widgets/global_shipping_chokepoint/assets/download_map.py
python crates/widgets/global_shipping_chokepoint/assets/build_map.py
```

The four sources are about 1.5 GiB and are **not committed**. The repository
root `.gitignore` ignores
`crates/widgets/global_shipping_chokepoint/assets/*.pbf`,
`crates/widgets/global_shipping_chokepoint/assets/*.zip`,
`crates/widgets/global_shipping_chokepoint/assets/*.part` and
`crates/widgets/global_shipping_chokepoint/assets/*.tmp`; the tracked files
here are this README, the two scripts and the generated `hormuz_map.bin`.

`download_map.py` downloads with the system `curl`
(`-L --fail --retry 3 -C -`), each file into `<name>.part`, renamed with
`os.replace` only after curl exits successfully — an interrupted run never
leaves a truncated file under the real name, and the `.part` it does leave
behind is ignored by git, so a partial download cannot be committed. A resume the
server refuses (curl exit 33 or 36) restarts from zero; every other curl failure
keeps the `.part` for the next run.

The script is idempotent: a file **at least** its recorded snapshot size is kept,
because a larger file cannot be a truncated download. Geofabrik rebuilds its
`-latest` extracts daily, so re-downloading 1.5 GB on every run because the
upstream data moved on would be worse than building from the newer extract; a
file larger than the snapshot is kept with a warning that the counts below no
longer apply. A file **smaller** than the snapshot is downloaded again, `--force`
downloads every file again (discarding any `.part` first, since a partial of the
previous snapshot must not be appended to), and `--sha256` prints every hash and
downloads a file again when its hash is not the recorded one. `--proxy` (default
`$HTTPS_PROXY`, then `$HTTP_PROXY`) is passed straight to curl, so
`--proxy socks5h://127.0.0.1:7890` works. It refuses to download with less than
2 GiB free, prints every file's byte size, and ends with the build command.

`build_map.py` checks the four files, refuses to run when one is missing, and
writes `hormuz_map.bin` through `<out>.tmp` + `os.replace`: it removes a stale
`.tmp` before writing and again if the write fails, so an interrupted build
leaves nothing behind. The same inputs produce a byte-identical file.

## Sources

| file | bytes | SHA256 | URL | snapshot | licence |
|---|---|---|---|---|---|
| `land-polygons-split-4326.zip` | 928,619,967 | `84d44f2df27e501c3e8eb83777bca314897660ecfce8d09f6b00c588072da163` | <https://osmdata.openstreetmap.de/download/land-polygons-split-4326.zip> | 2026-10-28 | ODbL |
| `gcc-states-latest.osm.pbf` | 256,396,961 | `363606bd76d12ce03f0e2854fc9b3c7c2c5ea5b170cbec454b85c44ad65bba2a` | <https://download.geofabrik.de/asia/gcc-states-latest.osm.pbf> | 2026-10-27 | ODbL |
| `iran-latest.osm.pbf` | 229,955,721 | `06d98c2b3dd4615b6cc29b7cc6a44705152a63be9e57fac67db3bbc456ce5a49` | <https://download.geofabrik.de/asia/iran-latest.osm.pbf> | 2026-10-27 | ODbL |
| `pakistan-latest.osm.pbf` | 157,522,517 | `79138552345d21cf5aa362f40b7a765e5f9bd7a1ba8cafc3f34735f3722c16d9` | <https://download.geofabrik.de/asia/pakistan-latest.osm.pbf> | 2026-10-27 | ODbL |

Total 1,572,495,166 bytes (1,499.6 MiB).

The three Geofabrik extracts are the 2026-10-27 snapshot: each PBF's header
records `osmosis_replication_timestamp = 2026-10-27T21:23:59Z`. The land zip's
own `README.txt` dates its data `2026-10-28T00:00:00Z` and says it holds
873,875 polygon features split into a grid with slight overlap.

Pakistan is there for the eastern edge: the map box reaches 61.875°E and the
Iran–Pakistan border is at 61.66°E, so the GCC and Iran extracts alone leave the
0.2° strip east of the border without roads or borders.

## When a source is missing

`build_map.py` checks the four files before it reads anything. For each missing
file it prints

```
missing <file>
```

and then, once, the line

```
run "python crates/widgets/global_shipping_chokepoint/assets/download_map.py" first
```

and exits with status 1. Against an empty directory:

```
$ python crates/widgets/global_shipping_chokepoint/assets/build_map.py --assets C:\temp\empty
missing land-polygons-split-4326.zip
missing gcc-states-latest.osm.pbf
missing iran-latest.osm.pbf
missing pakistan-latest.osm.pbf
run "python crates/widgets/global_shipping_chokepoint/assets/download_map.py" first
```

## Building

```powershell
python crates/widgets/global_shipping_chokepoint/assets/build_map.py
```

The command above uses every default. Two flags exist:

- `--assets DIR` — where the four sources are (default: the directory this
  script is in). An empty directory is enough to exercise the missing-file
  path without moving the real sources.
- `--out FILE` — the asset to write (default: `<assets>/hormuz_map.bin`).

Requires Python 3.10+ with `shapely>=2.2`, `pyosmium>=4.3` and `pyshp>=2.3`
(`python -m pip install shapely osmium pyshp`). Pillow is not needed to build.

### Size check

The snapshot sizes in the table are also recorded in `download_map.py`; a file
at least that size is kept there, and a larger one is kept with a warning that
the counts below no longer apply. Geofabrik rebuilds its `-latest` extracts
daily, so `build_map.py` does **not** demand exact equality either: it refuses
files that cannot be a complete download (a PBF below 20 MiB, the land zip
below 500 MiB) and otherwise prints

```
warning: <file> is N bytes, the recorded snapshot is M; the upstream data may be newer
```

and builds anyway. Every count below is for the 2026-10-27/28 snapshot; a newer
snapshot will change them.

## What was generated

`hormuz_map.bin`: **1,417,288 bytes** (1.35 MiB), SHA256
`3c68a81e436ac4afd1b583f1de49831659563359bb51d135ca0f1f76281aba05`.

| section | kind | points | pieces | cells | bytes |
|---|---|---|---|---|---|
| land | 1 | 25,446 | 854 | 0 | 16 + 203,568 + 10,248 = 213,832 |
| borders | 2 | 1,320 | 120 | 2,048 | 16 + 10,560 + 1,440 + 16,384 = 28,400 |
| roads | 3 | 87,790 | 38,025 | 2,048 | 16 + 702,320 + 456,300 + 16,384 = 1,175,020 |
| **file** | | | | | **36 + 213,832 + 28,400 + 1,175,020 = 1,417,288** |

**Land.** 2,009 shapes of the land shapefile intersect the map box; after the
union, clip, `make_valid`, `simplify(0.001, preserve_topology=True)` and
dropping polygons under 0.01 km², 854 polygons stay (950 are dropped) and
produce 854 closed rings / 25,446 points. The area is converted to km² before
the test: a raw square degree is about 11,200 km² here, so a `0.01` in raw
degrees would be a ~112 km² cutoff and drop almost every island. The rings
cover the whole map box: their extent is 47.801998 / 21.443001 / 61.875 /
30.042 (the `f32` values as stored), and there are no interior rings in this
box.

**Borders.** 1,214 `boundary=administrative` + `admin_level=2` ways in the three
extracts; 258 of them carry `maritime=yes` and are rejected, 42 ways appear in
two extracts and are merged. 75 pieces / 1,230 points intersect the box before
the grid cut; after `simplify(0.0005, preserve_topology=False)` and the per-cell
cut they are 120 pieces / 1,320 points. No border segment is shorter than 16 m.

**Roads.** 127,660 distinct `highway=motorway|trunk|primary` ways in the three
extracts (48,953 GCC + 57,105 Iran + 21,602 Pakistan; 36 further ways appear in
two extracts and are merged). 35,611 pieces / 83,087 points intersect the box;
after `simplify(0.0012, preserve_topology=False)` and the per-cell cut they are
38,025 pieces / 87,790 points. 2,584 road segments are shorter than 16 m.

**Road LODs**, recomputed from the packed section (triangles = 2 × (vertices −
pieces)):

| LOD | runtime filter | pieces | vertices | triangles |
|---|---|---|---|---|
| `E_MIN` | class 0, has `ref`, `length_m >= 1000` | 5,148 | 16,107 | 21,918 |
| `E_ALL` | class 0 | 18,914 | 44,560 | 51,292 |
| `DE` | all | 38,025 | 87,790 | 99,530 |

`E_MIN` drops 32,877 pieces. It leaves 6,529 dangling endpoints on 4,182 pieces.
If that reads badly at 1×, plan §8/R4 widens `E_MIN` to "has `ref` or `length_m
>= 10,000" — that is a runtime filter change, no rebuild needed.

12 land segments are shorter than 16 m. Plan §4.1 makes that the trigger for
lowering `TESSELLATION_TOLERANCE` from `5e-6` to `1e-6` and re-pinning the land
triangle count; it is the only build statistic the parser's tolerance depends
on. The rings are 25,446 points in 854 closed rings; a tessellator that drops
each ring's repeated closing point would emit `25,446 - 3 × 854 = 22,884`
triangles, and lyon emits **22,876** at `1e-6` — the count `src/scene.rs` pins.
The borders add 2,400 triangles. At 1× on the user's 1,404 × 900 pane one frame
draws **55,350** triangles with a full picture of 1,000 vessels, against the
75,000 ceiling. See "Runtime contract" below.

## Format: KHL2

Little-endian throughout; the header and every section are 4-byte aligned, and
the file is exactly the sum of its sections with no trailing bytes.

```
offset  size  field
0       4     magic "KHL2"
4       4     version: u32 = 1
8       16    bbox: west, south, east, north (f32, degrees)
24      4     grid_cols: u32 = 64
28      4     grid_rows: u32 = 32
32      4     section_count: u32 = 3
36      …     sections, in the order land (1), borders (2), roads (3)
```

Every section:

```
offset  size  field
0       4     kind: u32 (1 land, 2 borders, 3 roads)
4       4     point_count: u32
8       4     piece_count: u32
12      4     cell_count: u32        // land = 0; borders/roads = cols * rows = 2,048
16      …     points: point_count × (lon f32, lat f32)
…       …     pieces: piece_count × (start u32, len u16, flags u8, reserved u8, length_m u32)
…       …     cells: cell_count × (start u32, count u32)
```

- `start` is an index into the section's points; `len` is the point count (≤
  65,535, asserted by the build) and includes the closing point for a land ring.
- `flags` bit 0 is "primary", bit 1 is "the way carries a `ref`"; land and
  borders write 0. `length_m` is the piece's length in metres and is 0 for land
  and borders. `reserved` is 0.
- `cells` is the piece array cut into consecutive `(start, count)` ranges in
  row-major order. Every piece of a section with `cell_count > 0` belongs to
  exactly one cell, and the ranges partition the array. Land has no cell table:
  the parser tessellates the rings and bins the triangles by centroid at run
  time.
- The cut runs in `f64` and the stored coordinate is the nearest `f32`, so a
  piece can sit up to 1.9e-6° outside its cell box: 43 of the 120 border pieces
  and 2,310 of the 38,025 road pieces do (worst 1.892e-6°). That is 0.2 m and
  harmless — but a containment check needs a 2e-6° tolerance, not equality.
  There is no piece on a grid line, so no piece was packed into two cells.
- The grid covers the bbox with 64 columns × 32 rows: cell `row * 64 + col`,
  column 0 at the west edge, row 0 at the south edge, each cell
  0.2199° × 0.2687° (about 22 km × 30 km).
- A piece is a polyline in degrees. The parser projects it to Web Mercator with
  the same formula as `model::project`; nothing is pre-projected.

`length_m` is `length_degrees × 111,320 × cos(centroid latitude)` — **metres**, not
the `111.32` (kilometres) the plan's prose prints; with 111.32 every piece would
fall under the 1,000 m `E_MIN` threshold. It is recomputed **after** the per-cell
cut, so the cell boundary shortens it.

The header bbox is stored as `f32`, so reading it back gives
`47.801998138427734 / 21.44300079345703 / 61.875 / 30.04199981689453`. Those are
exactly the `f32` values of the four `47.802 / 21.443 / 61.875 / 30.042`
constants, so a comparison in `f32` (or after widening **both** sides from `f32`
to `f64`) is bit-equal and any epsilon passes. Compared against the *decimal*
literals instead, the west edge is off by **1.862e-6°** (south 7.9e-7°, east 0,
north 1.8e-7°), which is more than plan §4.1's 1e-6°: a comparison against `f64`
literals needs an epsilon above 1.9e-6° (use 1e-5°).

## Runtime contract

`src/scene.rs` parses the file once (a `OnceLock`), projects every point to Web
Mercator radians and tessellates the land; `src/paint.rs` builds the frame's ten
paths from it. What the widget does with this asset:

**Road LOD.** There is one D+E section; `model::road_filter` picks the level
from the clamped zoom, so no rebuild is needed when a threshold moves:

| zoom | filter | rule | triangles in the asset |
|---|---|---|---|
| `< 1.6` | `EMin` | class 0 (motorway/trunk), has `ref`, `length_m >= 1000` | 21,918 |
| `1.6 .. < 2.8` | `EAll` | class 0 | 51,292 |
| `>= 2.8` | `De` | everything, cut to the viewport by the cell table | 99,530 |

The two thresholds are a visual-density choice, not a performance limit. Roads
and borders are screen-space ribbons: 1.6 px for a border, 1.2 px for a
motorway or trunk, 1.0 px for a primary road; two triangles per segment, and no
casing.

**Land culling.** The rings tessellate into 22,876 triangles at `1e-6`. A frame
visits the cells its viewport touches plus one cell of margin, which covers
every triangle whose box spans at most two cells on each axis. The tessellator
also emits triangles spanning most of the map — the widest covers 55×20 cells,
23.8% of the map box — and those are 98.7% of the land's area, so bucketing
them by centroid would hide nearly all of it. The 3,389 too-wide triangles are
kept aside (`LandLayer::wide`) and tested against the viewport's own rectangle
every frame. Grid rows are latitude bands in the build **and** in the parser:
reading the row off Mercator `y` would land up to a quarter row away from the
cell the build wrote.

**Labels.** Nineteen names — five countries, twelve cities and two seas — are
elements over the canvas rather than glyphs in it, so they are read from the
catalogue, shaped by the platform font and drawn at the display's scale. Each one
is centred on its anchor: GPUI lays an absolute child out by its top-left corner
and has no transform to shift it, so the text sits in a fixed 240 × 20 px box
placed at `(x − 120, y − 10)`. The widest name either locale ships is
`UNITED ARAB EMIRATES`, about 141 px at `text_xs`, so the box holds every one of
them without wrapping. Cities are named from the fitted zoom (`CITY_LABEL_ZOOM`), and
`model::place_labels` fixes what a frame draws: countries, then seas, then cities,
each list in its declared order; a later anchor within 72 px of one already placed
is skipped, so the strait's crowded cities do not print into one pile at 1×
(Qeshm and Ras Al Khaimah are the two that go on a 1,404 × 900 pane), and a name
whose anchor is nearer the canvas edge than 80 px — half the widest name — is
dropped rather than cut in half. Zooming in separates the crowded ones again.

**Budget.** One frame with a full picture of 1,000 vessels — each at its most
expensive, an 8-segment dot for a vessel whose ship record has not arrived — and
the hover ring, on the user's 1,404 × 900 pane:

| view | triangles |
|---|---|
| fitted 1×, `EMin` | 55,350 |
| 1.6× over the map's middle, `EAll` (the worst view found) | 57,707 |
| 4× over Dubai, `De` | 44,276 |

The ceiling is 75,000 (`paint.rs`'s `FRAME_TRIANGLE_BUDGET`): 8.58 ms of fixed
frame cost plus 107 ns per triangle still fits a 16.65 ms frame.
`the_visible_triangle_budget_is_under_the_frame_budget` sweeps a 1.56:1, a wide
and a tall pane at 1×, 1.6×, 2.8× and 4× over the map's middle, Dubai and the
strait, and pins the worst count (57,707). They are model values from that
budget, not frame times measured on a machine.

**Theme.** Five map roles, all opaque and all read from the theme every frame by
`MapColors::resolve`: `chart.map_ocean`, `chart.map_land`, `chart.map_border`,
`chart.map_road` (motorway/trunk) and `chart.map_road_minor` (primary). They
follow the OSM Carto transport layer: paper land `#f2efe9`, water `#aad3df`
deepened to keep the coastline floor, the admin purple `#a875c9` for borders,
the motorway rose `#e892a2` toward its casing `#dc2a67`, and the primary amber
`#fcd6a4` toward `#a06b00`. Their floors are in
`roles.rs::contrast_floors`: border on land and on ocean 3:1, road on land 3:1
and on ocean 2:1 (a bridge or causeway over the water), minor road on land
1.5:1, land on ocean 1.5:1 (1.69 light, 1.68 dark), and the sea on
`surface.sunken` 1.5:1 (1.66 light, 1.71 dark). The three strokes carry a
further ten percent margin over their floors, and all five roles are opaque. No
map colour is hard-coded in the widget.

The paper land is 1.01:1 on the light chart face and 2.88:1 on the dark one: it
is the OSM page colour, and an OSM land cannot also clear a 1.5:1 step from the
face (a land that did would be a mid-tone beige, not paper). That is why the
face floor moved to the sea in the OSM palette, and why the map covers the pane
rather than fitting inside it — with a cover fit there is no band of chart face
inside the map's own rect for the land to dissolve into.

**Attribution.** The widget draws `© OpenStreetMap contributors`
(`© OpenStreetMap 贡献者` in Chinese) in the map's bottom-left corner, in
`text_xs` and `colors.text` — the muted step does not reach the 4.5:1 a
paragraph is held to on the land — and a click opens
<https://www.openstreetmap.org/copyright>. The legend is raised to 32 px above
the map's bottom edge so the credit has its own line, and the stale badge in the
top-left is nowhere near it.

## Licence and attribution (ODbL)

- The sources and `hormuz_map.bin` are © OpenStreetMap contributors, available
  under the [Open Database License](https://opendatacommons.org/licenses/odbl/).
- The app must show `© OpenStreetMap contributors` on the map, linking to
  <https://www.openstreetmap.org/copyright> (plan §4.5).
- `hormuz_map.bin` is a **derivative database** and is committed to this
  repository. If the repository is public, that copy — plus `build_map.py`,
  `download_map.py` and the URLs, sizes and hashes above — is the public copy
  ODbL §4.6 asks for. If it is private, a public download URL for
  `hormuz_map.bin` still has to be defined before release (plan §4.6 / P4).
- The source extracts are not redistributed: `download_map.py` points at their
  public URLs, and the recorded sizes and SHA256 values identify the exact
  snapshot the committed asset was built from.
