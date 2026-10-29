# `hormuz_map.bin`

The OSM vector base map the Hormuz vessel widget draws under its vessels: the
coastline, the land borders and the motorway/trunk/primary roads around the
Strait of Hormuz, packed into one KHL2 file and embedded with `include_bytes!`.
The coastline is packed three times — at 0.001°, 0.003° and 0.006° — and the
motorways and trunks twice — a fine copy and a coarse one — so a frame draws the
resolution its own scale justifies: the fitted zoom uses the 0.006° coast and the
coarse road copy, and zooming in brings the finer ones back.

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

`hormuz_map.bin`: **1,859,524 bytes** (1.77 MiB), SHA256
`00a93522723e9a543ec966263b5b60ccc95ca3279869faa63544122591556c3e`.

| section | kind | points | pieces | cells | bytes |
|---|---|---|---|---|---|
| land 0.001° | 1 | 25,446 | 854 | 0 | 16 + 203,568 + 10,248 = 213,832 |
| land 0.003° | 2 | 12,113 | 790 | 0 | 16 + 96,904 + 9,480 = 106,400 |
| land 0.006° | 3 | 8,099 | 785 | 0 | 16 + 64,792 + 9,420 = 74,228 |
| borders | 4 | 1,630 | 201 | 2,048 | 16 + 13,040 + 2,412 + 16,384 = 31,852 |
| roads | 5 | 108,505 | 45,728 | 2,048 | 16 + 868,040 + 548,736 + 16,384 = 1,433,176 |
| **file** | | | | | **36 + 213,832 + 106,400 + 74,228 + 31,852 + 1,433,176 = 1,859,524** |

**Land.** 2,009 shapes of the land shapefile intersect the map box; they are
unioned, clipped, made valid **once**, and that one geometry is then simplified
three times with `preserve_topology=True`, each tier dropping its own polygons
under 0.01 km² and packing one closed ring per polygon ring:

| tier | tolerance | ≈ metres | polygons kept / dropped | rings | points | triangles (lyon) |
|---|---|---|---|---|---|---|
| 0 | 0.001° | 100 m | 854 / 950 | 854 | 25,446 | 22,876 |
| 1 | 0.003° | 300 m | 790 / 1,014 | 790 | 12,113 | 9,736 |
| 2 | 0.006° | 600 m | 785 / 1,019 | 785 | 8,099 | 5,744 |

The area is converted to km² before the test: a raw square degree is about
11,200 km² here, so a `0.01` in raw degrees would be a ~112 km² cutoff and drop
almost every island. Tier 0's rings cover the whole map box: their extent is
47.801998 / 21.443001 / 61.875 / 30.042 (the `f32` values as stored), and there
are no interior rings in this box. Its section is byte-identical to the single
land section of the previous (version 1) asset, so four times the fitted zoom —
the view that draws tier 0 — paints exactly the geometry it painted before.

The `triangles` column is what `src/scene.rs` counts after lyon tessellates the
rings at a `1e-6` tolerance; the build's own estimate, `points − 3 × rings`,
reads 22,884 / 9,743 / 5,744. lyon skips a ring point that sits on the line
through its neighbours, and a simplified ring has a few of those (two at tier 0).
That is the **tier total**, not what a particular viewport draws: at the fitted
zoom on the design pane the 0.006° tier paints 5,733 of its 5,744 triangles — the
eleven missing are outside the cover fit's cropped rows.

**Worst deviation vs typical displacement.** `assets/build_map.py` prints the
discrete Hausdorff distance from tier 0's drawn land for every tier (shapely
`hausdorff_distance`, which samples the vertices):

| tier | worst distance | metres at this latitude | px @1,004 (1×) | px @ threshold |
|---|---|---|---|---|
| 0.003° | 0.004553° | 457 m | 0.45 px | 1.02 px at 450 m/px |
| 0.006° | 0.010406° | 1,043 m | **1.04 px** | **1.16 px at 900 m/px** |

The budget for these is not "strictly 1.0 px". A tier is allowed at most
`TIER_WORST_CASE_AT_THRESHOLD_PX` = 1.2 px at the scale where it starts being
drawn, and the tier the fitted view draws at most `TIER_WORST_CASE_AT_FIT_PX` =
1.1 px. The 0.006° tier is 1.04 px at 1× — 4% over the ideal one pixel — and that
4% is a deliberate trade: forcing it to 1.0 px means drawing the 0.003° tier at
1×, which costs about 4,000 more triangles and pushes the frame back into GPUI's
2^17 instance-buffer step, doubling that memory. The model test
`every_tier_stays_inside_the_pixel_budget_the_plan_sets` pins both budgets
against the measured constants, so the thresholds cannot drift away from the
measurements without failing.

These are **worst single points**, not typical displacement: the distance is one
vertex pair somewhere on a small island, and the rest of the coast moves by much
less. The pixel-level check on a real window is still a human step — see "Visual
acceptance" below.

**Islands.** The two coarser tiers drop small polygons of their own; the number
worth writing down is how many polygons tier 0 draws that the coarse tier no
longer does. Measured per source polygon — a tier-0 polygon counts as lost when
less than 0.01 km² of it is covered by the 0.006° tier's **kept** polygons (the
ones actually drawn, not the global `simplify` result before its filter) — tier
2 loses **77** of tier 0's 854 polygons; the largest of them is **0.1082 km²**
at 50.43684°E, 26.19868°N, 7 are at least 0.05 km² and none reaches 0.5 km².
Qeshm, Hormuz and Khasab are kept at every tier: the build checks a point on each
against every tier's kept polygons and prints `named places kept at <tier>: all`.

**Borders.** The ways are selected from the national relations, not from their
own tags: 19 `boundary=administrative` + `admin_level=2` relations across the
three extracts hold 5,184 distinct member ways, and 122 of the 1,034 border ways
that reach the pack carry no `admin_level` of their own — the Saudi–UAE land
border is made of ways like that, which is why selecting on the tag alone left
it out. A member way is dropped when it is `maritime=yes` (262 ways) or
`natural=coastline` (2); 42 ways appear in two extracts and are merged. The tag
filter cannot see a sea boundary that carries neither tag, so the selection has
a second, geometric rule: a candidate way is kept only when **at least one of
its points lies on the drawn land** (the packed **tier 0** land, grown by
0.002° — about 200 m, twice the land's own ~100 m simplification, so a border
point that is truly on the coast is not lost to the coastline's simplification).
The mask is tier 0 and not a coarser tier on purpose: deriving it from a 600 m
coast would move the drawn shoreline by up to a kilometre and drag the borders
with it. The test is "none of its points", never a share of them, so a border
that also crosses water survives: the two untagged Saudi–Bahrain connector ways
sit on the coast and stay, and the closest kept point anywhere (the Saudi–Qatar
connector) reads 57 m from the drawn coast. Two ways fail it in this snapshot —
way 1211286901 and way 1211287612, the UAE relation's "United Arab Emirates -
Abu Dhabi" rings 0.88 km and 1.49 km offshore of Abu Dhabi — and are dropped. 48
of the 122 relation-only ways reach the box (46 after that drop): 119 pieces /
1,466 points intersect the box before the grid cut; after `simplify(0.0005,
preserve_topology=False)` and the per-cell cut they are 201 pieces / 1,630
points. 3 border segments are shorter than 16 m. The King Fahd Causeway is not
affected by the mask: its national-border way is `maritime=yes` and was already
excluded by the tag filter, while the causeway *road* is a class-0 trunk and is
packed with the other roads.

**Roads.** 127,660 distinct `highway=motorway|trunk|primary` ways in the three
extracts (48,953 GCC + 57,105 Iran + 21,602 Pakistan; 36 further ways appear in
two extracts and are merged). 35,611 fine pieces / 83,087 points intersect the
box at `simplify(0.0012)`. The class-0 ways are then joined into the chains their
shared endpoints allow (5,999 chains) and simplified again to ~300 m, which is
what makes the coarse copy coarse: a two-node way cannot be simplified on its
own, and most ways are two-node. That gives 5,992 coarse pieces / 17,304 points.
After the per-cell cut the section holds 45,728 pieces / 108,505 points: 38,025
fine (motorway, trunk, primary) and 7,703 coarse. 716 fine and 372 coarse
segments are shorter than 16 m.

**Road LODs**, recomputed from the packed section (triangles = 2 × (vertices −
pieces)):

| LOD | runtime filter | pieces | vertices | triangles |
|---|---|---|---|---|
| `COARSE` | zoom < 1.6: the coarse copy, every piece | 7,703 | 20,715 | 26,024 |
| `FINE` | 1.6 ≤ zoom < 2.8: the fine copy of class 0, every piece | 18,914 | 44,560 | 51,292 |
| `DE` | zoom ≥ 2.8: the fine copy plus the primary roads | 38,025 | 87,790 | 99,530 |

No `ref` and no `length_m` test remains at any level: the coarse copy drops
nothing, which is what makes the network continuous at 1×, and the fine copy
drops nothing either. The two copies are disjoint, so no road is ever drawn
twice. The coarse copy leaves 629 dangling endpoints on 588 pieces, which are
the places a motorway or trunk leaves the map box or ends at a junction the
network does not continue.

Land segments shorter than 16 m: 12 at tier 0, 8 at tier 1 and 2 at tier 2. Plan
§4.1 makes the tier 0 count the trigger for lowering `TESSELLATION_TOLERANCE`
from `5e-6` to `1e-6` and re-pinning the land triangle count; it is the only
build statistic the parser's tolerance depends on, and the same `1e-6` is used
for all three tiers (a coarser tier's points are at least a kilometre apart, so
the tolerance only decides how lyon joins a ring against itself).

## Format: KHL2

Little-endian throughout; the header and every section are 4-byte aligned, and
the file is exactly the sum of its sections with no trailing bytes.

```
offset  size  field
0       4     magic "KHL2"
4       4     version: u32 = 2
8       16    bbox: west, south, east, north (f32, degrees)
24      4     grid_cols: u32 = 64
28      4     grid_rows: u32 = 32
32      4     section_count: u32 = 5
36      …     sections, in the order land 0.001° (1), land 0.003° (2),
              land 0.006° (3), borders (4), roads (5)
```

Every section:

```
offset  size  field
0       4     kind: u32 (1..3 land tiers, 4 borders, 5 roads)
4       4     point_count: u32
8       4     piece_count: u32
12      4     cell_count: u32        // the land tiers = 0; borders/roads = cols * rows = 2,048
16      …     points: point_count × (lon f32, lat f32)
…       …     pieces: piece_count × (start u32, len u16, flags u8, reserved u8, length_m u32)
…       …     cells: cell_count × (start u32, count u32)
```

Version 1 held one land section and two line sections, in the order 1, 2, 3.
This parser does not read it: `src/scene.rs` rejects any version but 2, so the
asset and the parser move together and there is no second layout to maintain.

- `start` is an index into the section's points; `len` is the point count (≤
  65,535, asserted by the build) and includes the closing point for a land ring.
- `flags` bit 0 is "primary", bit 1 is "the way carries a `ref`" (the fine copy
  only; packed as data, no longer read by the runtime) and bit 2 is "a piece of
  the coarse copy of class 0"; land and borders write 0. `length_m` is the
  piece's length in metres and is 0 for land and borders; the runtime no longer
  filters on it either. `reserved` is 0.
- `cells` is the piece array cut into consecutive `(start, count)` ranges in
  row-major order. Every piece of a section with `cell_count > 0` belongs to
  exactly one cell, and the ranges partition the array. Land has no cell table:
  the parser tessellates the rings and bins the triangles by centroid at run
  time.
- The cut runs in `f64` and the stored coordinate is the nearest `f32`, so a
  piece can sit up to 1.9e-6° outside its cell box: 76 of the 201 border pieces
  and 3,668 of the 45,728 road pieces do (worst 1.892e-6°). That is 0.2 m and
  harmless — but a containment check needs a 2e-6° tolerance, not equality.
  There is no piece on a grid line, so no piece was packed into two cells.
- The grid covers the bbox with 64 columns × 32 rows: cell `row * 64 + col`,
  column 0 at the west edge, row 0 at the south edge, each cell
  0.2199° × 0.2687° (about 22 km × 30 km).
- A piece is a polyline in degrees. The parser projects it to Web Mercator with
  the same formula as `model::project`; nothing is pre-projected.

`length_m` is `length_degrees × 111,320 × cos(centroid latitude)` — **metres**, not
the `111.32` (kilometres) the plan's prose prints; with 111.32 every piece would
be a thousand times too short. It is recomputed **after** the per-cell cut, so
the cell boundary shortens it. The runtime no longer filters on it: it is kept
because it is the build's own measure of a piece and what the LOD statistics
above are counted from.

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

**Road LOD.** There is one road section; `model::road_filter` picks the level from
the clamped zoom, so no rebuild is needed when a threshold moves. The motorways
and trunks are packed twice and the two copies are disjoint — the coarse one is
drawn only below the first threshold and the fine one only at or above it, so no
road is ever drawn twice. Neither level filters on a piece's tags or its length:

| zoom | filter | rule | triangles in the asset |
|---|---|---|---|
| `< 1.6` | `Coarse` | the coarse copy of class 0, every piece | 26,024 |
| `1.6 .. < 2.8` | `Fine` | the fine copy of class 0, every piece | 51,292 |
| `>= 2.8` | `De` | the fine copy plus the primary roads, cut to the viewport by the cell table | 99,530 |

The two thresholds are a visual-density choice, not a performance limit. Roads
and borders are screen-space ribbons: 1.6 px for a border, 1.4 px for a motorway
or trunk, 1.2 px for a primary road; two triangles per segment, and no casing. A
border is dashed as well as violet — 5 px on and 3 px off, walked along the
piece in `paint.rs` — so the three kinds of line are told apart by pattern as
well as by hue: a dashed violet border, a solid rose motorway and a solid ochre
primary road.

**Land tiers.** The three land sections are the same coastline at 0.001°, 0.003°
and 0.006°; `model::land_tier` picks one from the projection's **metres per
pixel**, never from the zoom number, because a zoom is relative to a fit that
changes with the pane's size. The thresholds are 450 and 900 m/px: at and above
900 m/px the 0.006° tier is drawn, 450–900 m/px the 0.003° one, below 450 m/px
the 0.001° one. What is guarded is the error those thresholds produce:
`model.rs` holds the measured `TIER_WORST_CASE_METRES = [0, 457, 1,043]` metres
from the build's Hausdorff measurement and two pixel budgets — 1.2 px at the
scale a tier starts being drawn, 1.1 px at the fitted zoom — and
`every_tier_stays_inside_the_pixel_budget_the_plan_sets` fails if a threshold, a
tier's tolerance or the build's simplification moves the numbers past them. On
the user's 1,404 × 900 pane the fitted zoom is 1,004 m/px, so **1× draws the
0.006° tier** (5,733 of its 5,744 triangles instead of 22,844 of 22,876); 1.6×
draws 0.003°; 2.8× and 4× draw 0.001° — the same geometry, triangle for triangle,
that the map drew before the tiers existed. A narrow pane fits fewer pixels
across the same degrees and therefore reads coarser at the same zoom: 4× on a
320 × 400 pane is 598 m/px and still draws the 0.003° tier.

**Land culling.** A tier's rings tessellate at `1e-6` into the triangles the
table above counts. A frame visits the cells its viewport touches plus one cell
of margin, which covers every triangle whose box spans at most two cells on each
axis. The tessellator also emits triangles spanning most of the map — at tier 0
the widest covers 55×20 cells, 23.8% of the map box — and those are 98.7% of the
land's area, so bucketing them by centroid would hide nearly all of it. The
too-wide triangles (3,389 / 1,591 / 958, tier by tier) are kept aside
(`LandLayer::wide`) and tested against the viewport's own rectangle every frame.
Grid rows are latitude bands in the build **and** in the parser: reading the row
off Mercator `y` would land up to a quarter row away from the cell the build
wrote.

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

**Budget.** One frame with no vessels on the user's 1,404 × 900 pane, which is
the frame the plan's §8 target is about:

| layer | triangles |
|---|---|
| land, 0.006° tier (was 22,844 drawn of 22,876) | 5,733 |
| borders (dashed) | 2,460 |
| roads, `Coarse` copy | 26,022 |
| TSS lanes (dashed) | 142 |
| **fitted 1× frame** | **34,357** |

That is 51,468 → 34,357 triangles, a 33% cut, measured on the **same viewport**:
the old frame on this pane is 22,844 land + 2,460 + 26,022 + 142. The land's own
share fell 44% → 17%.

The plan's §8 targets for the fitted frame are **≤ 21,000 triangles** and
**≤ 65,536 vertices**, and neither is met: the target is a budget for P1 and P2
together — about 6,000 for the land plus at most 14,900 for a road network
thinned by P2 — while the road layer here is still the 26,024 triangles P1
deliberately leaves alone. The frame emits 103,071 vertices, 78,066 of them the
road network's. `the_fitted_frame_is_pinned_until_p2` pins the exact count as a
change detector; **P2 must replace that pin with `<= 21_000`**, and the vertex
gate closes with the same road thinning.

With a full picture of 1,000 vessels — each at its most expensive, an 8-segment
dot for a vessel whose ship record has not arrived — and the hover ring, the
worst view the sweep finds is **51,355 triangles**, at 2.8× over the map's middle
on the 1.56:1 pane. That is the plan's **worst-frame target of ≤ 27,000** missed
by 90%; it needs P2's roads and P4's vessel dots together, and neither is part of
P1. The worst view moved to 2.8× because that is where the land switches back to
the 0.001° tier and the road layer switches to D+E in the same frame. The
ceiling for a 60 Hz frame is 75,000 (`paint.rs`'s `FRAME_TRIANGLE_BUDGET`):
8.58 ms of fixed frame cost plus 107 ns per triangle still fits a 16.65 ms
frame. `the_visible_triangle_budget_is_under_the_frame_budget` sweeps a 1.56:1,
a wide and a tall pane at 1×, 1.6×, 2.8× and 4× over the map's middle, Dubai and
the strait, and pins the worst count (51,355). They are model values from that
budget, not frame times measured on a machine.

**Visual acceptance (P1).** The 0.006° tier's worst measured deviation is 1.04 px
at the fitted zoom (see "Worst deviation vs typical displacement" above), which
is over the plan's one-pixel rule by 4% worst-case and typical coastline
displacement is much smaller. The LOD change has not yet been checked on a real
window: the human step is a 1× screenshot of the strait (Qeshm, Hormuz, Khasab
and the small islands around them) against the previous build, to be recorded
here before P1 is signed off. Until then this README states the measurement, not
a visual pass.

**Theme.** Five map roles, all opaque and all read from the theme every frame by
`MapColors::resolve`: `chart.map_ocean`, `chart.map_land`, `chart.map_border`,
`chart.map_road` (motorway/trunk) and `chart.map_road_minor` (primary). They are
the OSM Carto transport layer's own colours: paper land `#f2efe9`, water
`#aad3df`, the admin violet `#a875c9` for borders, the motorway casing `#dc2a67`
and the primary casing `#a06b00`. Their floors are in
`roles.rs::contrast_floors`: every stroke reads 2.5:1 on the land and 2.0:1 on
the sea (a border crosses the water, a bridge or causeway carries a road over
it). Measured, light / dark: border 3.02 / 2.16 and 3.56 / 6.00, motorway 4.00 /
2.86 and 3.56 / 6.00, primary 3.98 / 2.84 and 3.24 / 5.45. All five roles are
opaque. No map colour is hard-coded in the widget.

The two fills are the OSM pair and read 1.40:1 against each other in the light
appearance (1.68 dark): below the 1.5:1 the design asked for, which is why both
fill floors are pinned at 1.35:1 — the alternatives were a deeper sea, which is
not the OSM colour, or a land that is not paper. The paper land is 1.01:1 on the
light chart face and 2.88:1 on the dark one, so the sea is the fill held against
the face (1.38 light, 1.71 dark). That is also why the map covers the pane
rather than fitting inside it: with a cover fit there is no band of chart face
inside the map's own rect for the land to dissolve into.

The land-on-face separation was **deliberately dropped** — the palette's owner
chose the OSM paper land over a visible map-to-pane step, so the paper land
stays at 1.01:1 against `surface.sunken` with no hairline border, no darker land
and no moved face. The measurement is kept here because it is the cost of the
choice: anything that clears 1.5:1 there is no longer the OSM colour, and the
fill floors do not cover it (`roles.rs` records the same in its table comment).

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
