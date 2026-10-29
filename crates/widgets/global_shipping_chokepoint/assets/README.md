# `hormuz_map.bin`

The OSM vector base map the Hormuz vessel widget draws under its vessels: the
coastline, the land borders and the motorway/trunk/primary roads around the
Strait of Hormuz, packed into one KHL2 file and embedded with `include_bytes!`.
The coastline is packed four times — at 0.001°, 0.002°, 0.003° and 0.006° — and
the motorways and trunks three times: every merged chain at 0.002° (the fine
copy), the chains at least 0.3 km long at 0.004° (the middle copy) and the same
long chains at 0.008° (the coarse copy). A short chain is feature-dropped at
coarse scales and comes back through the fine copy below 371 m/px, so a frame
draws the resolution its own scale justifies: the fitted zoom uses the 0.006°
coast and the coarse road copy without the short chains, and zooming in brings
the finer coast and the middle copy, then every chain. The primary roads are
joined into chains too, with the same 0.3 km drop, and are drawn only at the
closest zoom (4×).

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

`hormuz_map.bin`: **1,321,456 bytes** (1.26 MiB), SHA256
`081d610a738c55ac1f1c038701af78796a71f3bcb42495df7aed7b29e7ee1dbf`.
Two consecutive builds are byte-identical.

| section | kind | points | pieces | cells | bytes |
|---|---|---|---|---|---|
| land 0.001° | 1 | 25,446 | 854 | 0 | 16 + 203,568 + 10,248 = 213,832 |
| land 0.002° | 2 | 15,767 | 805 | 0 | 16 + 126,136 + 9,660 = 135,812 |
| land 0.003° | 3 | 12,113 | 790 | 0 | 16 + 96,904 + 9,480 = 106,400 |
| land 0.006° | 4 | 8,099 | 785 | 0 | 16 + 64,792 + 9,420 = 74,228 |
| borders | 5 | 1,630 | 201 | 2,048 | 16 + 13,040 + 2,412 + 16,384 = 31,852 |
| roads | 6 | 60,240 | 21,748 | 2,048 | 16 + 481,920 + 260,976 + 16,384 = 759,296 |
| **file** | | | | | **36 + 213,832 + 135,812 + 106,400 + 74,228 + 31,852 + 759,296 = 1,321,456** |

**Land.** 2,009 shapes of the land shapefile intersect the map box; they are
unioned, clipped, made valid **once**, and that one geometry is then simplified
four times with `preserve_topology=True`, each tier dropping its own polygons
under 0.01 km² and packing one closed ring per polygon ring:

| tier | tolerance | ≈ metres | polygons kept / dropped | rings | points | triangles (lyon) |
|---|---|---|---|---|---|---|
| 0 | 0.001° | 100 m | 854 / 950 | 854 | 25,446 | 22,876 |
| 1 | 0.002° | 200 m | 805 / 999 | 805 | 15,767 | 13,347 |
| 2 | 0.003° | 300 m | 790 / 1,014 | 790 | 12,113 | 9,736 |
| 3 | 0.006° | 600 m | 785 / 1,019 | 785 | 8,099 | 5,744 |

The area is converted to km² before the test: a raw square degree is about
11,200 km² here, so a `0.01` in raw degrees would be a ~112 km² cutoff and drop
almost every island. Tier 0's rings cover the whole map box: their extent is
47.801998 / 21.443001 / 61.875 / 30.042 (the `f32` values as stored), and there
are no interior rings in this box. Its section is byte-identical to the single
land section of the previous (version 1) asset, so four times the fitted zoom —
the view that draws tier 0 — paints exactly the geometry it painted before.

The `triangles` column is what `src/scene.rs` counts after lyon tessellates the
rings at a `1e-6` tolerance; the build's own estimate, `points − 3 × rings`,
reads 22,884 / 13,352 / 9,743 / 5,744. lyon skips a ring point that sits on the
line through its neighbours, and a simplified ring has a few of those (two at
tier 0). That is the **tier total**, not what a particular viewport draws: at the
fitted zoom on the design pane the 0.006° tier paints 5,733 of its 5,744
triangles — the eleven missing are outside the cover fit's cropped rows.

**Worst deviation vs typical displacement.** `assets/build_map.py` prints the
discrete Hausdorff distance from tier 0's drawn **boundary** for every tier, on
the same local equirectangular plane `length_metres` uses (`hypot(Δlon·cos(lat),
Δlat) × 111,320`), and the pair of points that attains it:

| tier | worst pair (lon, lat) → (lon, lat) | local metres | px @1,004 (1×) | px @ threshold |
|---|---|---|---|---|
| 0.002° | (55.6096736, 25.5710723) → (55.6105254, 25.5740735) | **344.8 m** | 0.34 px | **1.197 px at 288 m/px** |
| 0.003° | (53.6230740, 26.6894615) → (53.6244208, 26.6938176) | 503.4 m | 0.50 px | 1.119 px at 450 m/px |
| 0.006° | (50.3324772, 25.5430598) → (50.3241615, 25.5367125) | 1,092.9 m | **1.09 px** | **1.1996 px at 911 m/px** |

The budget for these is not "strictly 1.0 px". A tier is allowed at most
`LAND_TIER_WORST_CASE_AT_THRESHOLD_PX` = 1.2 px at the scale where it starts
being drawn, and the tier the fitted view draws at most
`LAND_TIER_WORST_CASE_AT_FIT_PX` = 1.1 px. The 0.006° tier is 1.09 px at 1× — 9%
over the ideal one pixel — and that 9% is a deliberate trade: forcing it to
1.0 px means drawing the 0.003° tier at 1×, which costs about 4,000 more
triangles and pushes the frame back into GPUI's 2^17 instance-buffer step,
doubling that memory. The thresholds are derived from those measurements:
**288** is `344.8 / 1.2 = 287.3` rounded up, **911** is `1,092.9 / 1.2 = 910.8`
rounded up, and **450** is a round-up of `503.4 / 1.2 = 419.5` that leaves 6.7%
of the budget unused. The model test
`every_tier_stays_inside_the_pixel_budget_the_plan_sets` checks both budgets and
the derivation (`threshold ≥ worst error / 1.2`) against the measured constants,
so the thresholds cannot drift away from the measurements without failing.

The earlier figures — 312.4 / 456.5 / 1,043.4 m and thresholds 261 / 450 / 900 —
were computed by scaling the **degree** Hausdorff distance by the longitude axis
(`111,320 × cos(lat)`), which understates a mostly north-south pair. The 0.002°
tier's worst pair moves 0.0030° north but only 0.00085° east; its ground
distance is 344.8 m, not 312.4 m, and at 261 m/px it would have been 1.32 px.
The build now measures the pair on the local plane and prints it, and the
thresholds above are the derived ones.

These are **worst single points**, not typical displacement: the distance is one
vertex pair somewhere on a small island, and the rest of the coast moves by much
less. The pixel-level check on a real window is still a human step — see "Visual
acceptance" below.

**Islands.** The coarser tiers drop small polygons of their own: 999 polygons
under 0.01 km² at 0.002°, 1,014 at 0.003° and 1,019 at 0.006°. The number worth
writing down is how many polygons tier 0 draws that the coarse tier no longer
does. Measured per source polygon — a tier-0 polygon counts as lost when less
than 0.01 km² of it is covered by the 0.006° tier's **kept** polygons (the ones
actually drawn, not the global `simplify` result before its filter) — tier 3
loses **77** of tier 0's 854 polygons; the largest of them is **0.1082 km²** at
50.43684°E, 26.19868°N, 7 are at least 0.05 km² and none reaches 0.5 km².
Qeshm, Hormuz and Khasab are kept at every tier: the build checks a point on
each against every tier's kept polygons and prints `named places kept at <tier>:
all` four times.

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
two extracts and are merged). The primary roads are joined into the chains their
shared endpoints allow too (11,364 chains), simplified at 0.002° and cut with the
same 0.3 km feature drop: 4,439 chains / 12,966 points / 17,054 triangles before
the grid cut, 5,343 pieces / 14,768 vertices / 18,850 triangles after it. The
primary layer is drawn only at the closest zoom (4×), where 0.3 km is 1.2 px, so
a dropped primary chain has no coarser scale at which it would come back: the
drop is build-time, unlike class 0's runtime gate. 6,925 of its 11,364 chains are
dropped.

The motorways and trunks are joined into the chains their shared endpoints allow
(5,999 chains), and **all three** copies are built from those chains: a two-node
way cannot be simplified on its own, and most ways are two-node, so joining
first is what makes every copy cheap. The fine copy stores every chain at 0.002°
(18,834 points before the cut), the middle copy stores the chains at least
**0.3 km** long at 0.004° (2,721 chains, 9,271 points, 6,550 segments) and the
coarse copy stores the same long chains at 0.008° (2,721 chains, 7,478 points,
4,757 segments). The 3,278 short chains are **feature-dropped** from the middle
and coarse copies and flagged in the fine copy. The fine copy is the only one
that holds them, and the runtime draws it only below the middle copy's 371 m/px
threshold, so the short chains come back at 371 m/px — where 0.3 km is 0.8 px —
rather than at the fine copy's own 628 m/px gate. That gate is still enforced (a
fine record is never drawn above 628 m/px), but the middle copy's switch is the
binding scale. Dropping a whole chain keeps every drawn road continuous;
dropping a piece of a longer chain would not.

The fine copy's tolerance was 0.0012° before P2; P2 raises it to **0.002°** as
well. The plan authorized the coarse copy only, but the fine copy's per-way
0.0012° geometry was the other half of the 2.8× frame, and the chain-level 0.002°
is 0.8 px at 4×. P4 adds the **0.004° middle copy** for the 371–742 m/px band,
where the fine copy was the heaviest layer in the frame and the coarse copy would
be more than 1.2 px off.

The 0.3 km threshold is the build's feature-drop rule. The histogram below is for
all class-0 chains; the 0.3 km cut drops 3,278 chains, and the middle and coarse
copies keep 2,721 chains. The primary copy uses the same cut: 6,925 of 11,364
chains are dropped, leaving 4,439 chains / 12,966 points / 17,054 triangles
before the grid cut.

The chain-length distribution (coarse segments are after the 0.008°
simplification, before the grid cut):

| length | chains | source segments | coarse segments | coarse triangles |
|---|---|---|---|---|
| 0–1 km | 3,894 | 24,586 | 4,019 | 8,038 |
| 1–2 km | 412 | 8,759 | 426 | 852 |
| 2–5 km | 614 | 25,022 | 653 | 1,306 |
| 5–10 km | 422 | 27,998 | 531 | 1,062 |
| 10–20 km | 267 | 29,100 | 510 | 1,020 |
| 20+ km | 390 | 119,650 | 1,983 | 3,966 |

After the per-cell cut the section holds **21,748 pieces / 60,240 points**:
5,343 primary, 7,728 fine class-0, 4,389 middle and 4,288 coarse. 413 fine
class-0, 23 middle and 20 coarse segments are shorter than 16 m.

**Road LODs**, recomputed from the packed section (triangles = 2 × (vertices −
pieces)):

| LOD | runtime rule | pieces | vertices | triangles |
|---|---|---|---|---|
| `COARSE` | scale ≥ 742.1 m/px: the long chains at 0.008° | 4,288 | 10,596 | 12,616 |
| `MIDDLE` | 371.1 ≤ scale < 742.1 m/px and zoom < 4×: the long chains at 0.004° | 4,389 | 12,593 | 16,408 |
| `FINE` | scale < 371.1 m/px and zoom < 4×: every class-0 chain, short ones gated | 7,728 | 22,283 | 29,110 |
| `DE` | scale < 371.1 m/px and zoom ≥ 4×: the fine copy plus the primary roads | 13,071 | 37,051 | 47,960 |
| `PRIMARY` | drawn only inside `DE`, at 4×: the chains at least 0.3 km long | 5,343 | 14,768 | 18,850 |

The coarse and middle copies are not a partition of class 0: they are second and
third geometries of the long chains the fine copy also holds. The three copies
are still mutually exclusive at runtime — the coarse filter draws only coarse
records, the middle one only middle records and the fine one only fine records —
so no road is ever drawn twice. The coarse copy leaves 2,450 dangling endpoints
on 1,866 pieces; many of them are junctions whose short connector is one of the
dropped chains, which is the visible cost of the feature dropping at 1×.

Land segments shorter than 16 m: 11 at tier 0, 7 at tier 1, 7 at tier 2 and 1 at
tier 3. Plan §4.1 makes the tier 0 count the trigger for lowering
`TESSELLATION_TOLERANCE` from `5e-6` to `1e-6` and re-pinning the land triangle
count; it is the only build statistic the parser's tolerance depends on, and the
same `1e-6` is used for all four tiers (a coarser tier's points are at least a
kilometre apart, so the tolerance only decides how lyon joins a ring against
itself).

## Format: KHL2

Little-endian throughout; the header and every section are 4-byte aligned, and
the file is exactly the sum of its sections with no trailing bytes.

```
offset  size  field
0       4     magic "KHL2"
4       4     version: u32 = 3
8       16    bbox: west, south, east, north (f32, degrees)
24      4     grid_cols: u32 = 64
28      4     grid_rows: u32 = 32
32      4     section_count: u32 = 6
36      …     sections, in the order land 0.001° (1), land 0.002° (2),
              land 0.003° (3), land 0.006° (4), borders (5), roads (6)
```

Every section:

```
offset  size  field
0       4     kind: u32 (1..4 land tiers, 5 borders, 6 roads)
4       4     point_count: u32
8       4     piece_count: u32
12      4     cell_count: u32        // the land tiers = 0; borders/roads = cols * rows = 2,048
16      …     points: point_count × (lon f32, lat f32)
…       …     pieces: piece_count × (start u32, len u16, flags u8, reserved u8, length_m u32)
…       …     cells: cell_count × (start u32, count u32)
```

Version 1 held one land section and two line sections, version 2 three land
sections and two line sections, version 3 four land sections and two line
sections. This parser does not read an older asset: `src/scene.rs` rejects any
version but 3, so the asset and the parser move together and there is no second
layout to maintain.

- `start` is an index into the section's points; `len` is the point count (≤
  65,535, asserted by the build) and includes the closing point for a land ring.
- `flags` bit 0 is "primary", bit 1 is "a piece of the middle copy of class 0",
  bit 2 is "a piece of the coarse copy of class 0" and bit 3 is "a piece of a
  class-0 chain shorter than 0.3 km", which the runtime gates by scale. Bits 4
  and above are undefined and the parser rejects them. Land and borders write 0.
  `length_m` is the piece's length in metres and is 0 for land and borders; the
  runtime no longer filters on it either. `reserved` is 0.
- `cells` is the piece array cut into consecutive `(start, count)` ranges in
  row-major order. Every piece of a section with `cell_count > 0` belongs to
  exactly one cell, and the ranges partition the array. Land has no cell table:
  the parser tessellates the rings and bins the triangles by centroid at run
  time.
- The cut runs in `f64` and the stored coordinate is the nearest `f32`, so a
  piece can sit up to 1.9e-6° outside its cell box: 76 of the 201 border pieces
  and 4,856 of the 21,748 road pieces do (worst 1.892e-6°). That is 0.2 m and
  harmless — but a containment check needs a 2e-6° tolerance, not equality.
  Five border points and 106 road points sit exactly on a grid line; the cell
  table still assigns every piece to exactly one cell, so no piece is packed
  into two cells.
- The grid covers the bbox with 64 columns × 32 rows: cell `row * 64 + col`,
  column 0 at the west edge, row 0 at the south edge, each cell
  0.2199° × 0.2687° (about 22 km × 30 km).
- A piece is a polyline in degrees. The parser projects it to Web Mercator with
  the same formula as `model::project`; nothing is pre-projected.

`length_m` is the piece's length on a local equirectangular projection at its
reference latitude: `Σ hypot(Δlon × cos(lat), Δlat) × 111,320`, in **metres**.
The two axes are not interchangeable; the old formula used the longitude scale
for both and underestimated a north-south piece by 11%. A cell-cut fragment
inherits its source's reference latitude, so the fragments' lengths sum to the
source length. `assert_chain_partition` uses that to prove each copy's cut
pieces rejoin to that copy's own chain geometry — the fine copy, which holds
every chain; the middle and coarse copies, which hold the long ones; and the
primary copy, which holds its own — and the 0.3 km feature drop is decided on the
same unique length. The runtime does not filter on `length_m`: only the border
validation (which requires zero) and the tests read it; it is kept because it is
the build's own measure of a piece and what the LOD statistics above are counted
from.

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

**Road LOD.** There is one road section; `model::road_filter(metres_per_pixel,
zoom)` picks the level, so no rebuild is needed when a threshold moves. The
class-0 copies are chosen by the picture's real scale, the primary layer by the
zoom, and a short chain by a second scale gate:

| condition | filter | rule | triangles in the asset |
|---|---|---|---|
| scale ≥ 742.1 m/px | `CoarseCopy` | the coarse copy: the long chains at 0.008° | 12,616 |
| 371.1 ≤ scale < 742.1 m/px, zoom < 4× | `MiddleCopy` | the middle copy: the same long chains at 0.004° | 16,408 |
| 371.1 ≤ scale < 742.1 m/px, zoom ≥ 4× | `MiddleCopyAndPrimary` | the middle copy plus the primary roads | 35,258 |
| scale < 371.1 m/px, zoom < 4× | `FineCopy` | every class-0 chain; short ones only at ≤ 628 m/px | 29,110 |
| scale < 371.1 m/px, zoom ≥ 4× | `FineCopyAndPrimary` | the fine copy plus the primary roads (18,850 of them) | 47,960 |

The coarse threshold is derived from the build's tolerance on its **worst axis**:
0.008° is 890.6 m on the latitude axis (a degree of latitude is 111,320 m, while
a degree of longitude at the map's scale latitude is 100,160 m), and
890.6 / 1.2 = 742.1 m/px is the coarsest scale at which the copy is still within
1.2 px. The middle threshold is derived the same way from 0.004°: 445.3 / 1.2 =
371.1 m/px. A 2,048 px wide pane is 688 m/px at 1×, below the coarse threshold
but above the middle one, so it draws the middle copy at the fitted zoom; the
design pane's 1,004 m/px still draws the coarse copy. The primary layer is a
visual-density choice, not a tolerance one, so it is gated by zoom: 4× is the
closest zoom the camera reaches, which is the product's literal "draw D+E at the
closest zoom". It is deliberately a zoom and not a scale: at 251 m/px (the design
pane's 4×) a tall pane would draw it from 2.7×, where it covers most of the
picture and costs more than the whole budget. The middle and coarse copies hold
only the long chains; the fine copy is the only one that holds a short chain, and
it is drawn only below 371.1 m/px, so the short chains come back there (0.3 km =
0.8 px). The fine copy's own 628 m/px gate is still enforced but no longer
binding. The primary copy is also chain-built and has the same 0.3 km drop,
applied at build time: the primary layer only exists at 4×, where 0.3 km is
1.2 px, so a dropped primary chain has no coarser scale at which to return.

The class-0 copies are mutually exclusive at runtime: the coarse filter draws
only coarse records, the middle one only middle records and the fine one only
fine records, so no road is drawn twice. Neither runtime level filters on a
piece's tags or its length. Roads and borders are screen-space ribbons: 1.6 px
for a border, 1.4 px for a motorway or trunk, 1.2 px for a primary road; two
triangles per segment, and no casing. A border is dashed as well as violet — 6 px
on and 4 px off, walked along the piece in `paint.rs` — so the three kinds of
line are told apart by pattern as well as by hue: a dashed violet border, a solid
rose motorway and a solid ochre primary road.

**Land tiers.** The four land sections are the same coastline at 0.001°, 0.002°,
0.003° and 0.006°; `model::land_tier` picks one from the projection's **metres
per pixel**, never from the zoom number, because a zoom is relative to a fit that
changes with the pane's size. The thresholds are 288, 450 and 911 m/px: at and
above 911 m/px the 0.006° tier is drawn, 450–911 m/px the 0.003° one, 288–450
the 0.002° one and below 288 m/px the 0.001° one. What is guarded is the error
those thresholds produce: `model.rs` holds the measured
`LAND_TIER_WORST_CASE_METRES = [0, 344.8, 503.4, 1,092.9]` metres on the local
plane from the build's Hausdorff measurement and two pixel budgets — 1.2 px at
the scale a tier starts being drawn, 1.1 px at the fitted zoom — and
`every_tier_stays_inside_the_pixel_budget_the_plan_sets` fails if a threshold, a
tier's tolerance or the build's simplification moves the numbers past them. The
288 and 911 thresholds are the exact `344.8 / 1.2` and `1,092.9 / 1.2` rounded
up; the 450 is a round-up of `503.4 / 1.2 = 419.5` that leaves 6.7% of the
budget unused. On the user's 1,404 × 900 pane the fitted zoom is 1,004 m/px, so
**1× draws the 0.006° tier** (5,733 of its 5,744 triangles instead of 22,844 of
22,876); 1.6× draws 0.003°; 2.8× draws 0.002°; 3.49× is 287.7 m/px — just below
the 288 switch — and draws 0.001°; 4× draws 0.001° too. The 0.001° geometry is
the same one, triangle for triangle, that the map drew before the tiers existed.
A narrow pane fits fewer pixels across the same degrees and therefore reads
coarser at the same zoom: 4× on a 320 × 400 pane is 598 m/px and still draws the
0.003° tier.

**Land culling.** A tier's rings tessellate at `1e-6` into the triangles the
table above counts. A frame visits the cells its viewport touches plus one cell
of margin, which covers every triangle whose box spans at most two cells on each
axis. The tessellator also emits triangles spanning most of the map — at tier 0
the widest covers 55×20 cells, 23.8% of the map box — and those are 98.7% of the
land's area, so bucketing them by centroid would hide nearly all of it. The
too-wide triangles (3,389 / 2,070 / 1,591 / 958, tier by tier) are kept aside
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

**Budget.** Two frames on the user's 1,404 × 900 pane: the empty fitted frame the
plan's §8 target is about, and the typical frame with 1,000 vessels whose ship
record has arrived (one marker triangle each), which has to stay in the 2^16
instance-buffer step:

| layer | empty 1× | typical 1× |
|---|---|---|
| land, 0.006° tier | 5,733 | 5,733 |
| borders (dashed 6/4) | 2,312 | 2,312 |
| roads, coarse copy of the chains ≥ 0.3 km | 12,614 | 12,614 |
| TSS lanes (dashed) | 142 | 142 |
| vessels (1,000 single triangles) | 0 | 1,000 |
| **fitted 1× frame** | **20,801** (62,403 vertices) | **21,801** (65,403 vertices) |

The empty frame is inside §8's **≤ 21,000 triangles / ≤ 65,536 vertices** with
199 triangles and 3,133 vertices to spare. The typical frame is inside the
**2^16 step** (≤ 21,845 triangles / ≤ 65,536 vertices) with only **44 triangles
and 133 vertices** to spare — the margin the 6/4 border dash bought. The history
on the same viewport is 51,468 (P0) → 34,357 (P1) → **20,801** (P2/P4), a 60% cut
from the baseline. The land is 28% of the empty frame now, down from 44% of the
P0 baseline. The coarse LOD's 12,616 in the table above is the packed geometry;
the frame draws 12,614 because two segments collapse to zero length on screen and
`push_quad` skips them.

With a full picture of 1,000 vessels — each at its most expensive, a six-segment
dot for a vessel whose ship record has not arrived — and the hover ring, the
worst view the sweep finds is **31,815 triangles**, at the **2,048 px wide pane
at 1.8648× over 25.879N, 52.799E**:

| layer | triangles |
|---|---|
| land, 0.002° tier | 10,721 |
| borders | 850 |
| roads, fine copy | 13,916 |
| TSS lanes | 280 |
| vessels (1,000 × 6 segments) | 6,000 |
| hover ring | 48 |
| **worst frame** | **31,815** |

That is **4,815 over the plan's worst-frame target of ≤ 27,000**; P4 records the
frame rather than claiming it. The frame sits just below the 0.004° middle road
copy's 371.1 m/px switch: at 369 m/px the fine road copy is drawn (13,916), while
the next band up would draw the middle copy. It is the road LOD cliff, not a
land threshold. The sweep's 7 × 5 centre grid plus two 9 × 9 refinement rounds
found it; the review's 2,048 × 1× frame at 27.057N is **31,149**, 666 lower. The
0.002° land tier and the six-segment dots took the old 2,048 × 1.539× frame from
39,438 to 24,310, and the 0.004° middle road copy took the 2,048 × 1× frame from
33,267 (fine copy) to 28,945. The wide 1,600 pane's fitted 1× frame is **29,668**
with the coarse road copy (11,652) and the 0.003° land (9,642). The next lever is
a finer middle road copy (0.003°) for the 278–371 m/px band, or a lower 0.004°
threshold — 369 m/px would be 1.204 px, 0.3% over the 1.2 px budget, so it is not
acceptable. A fifth land tier at 725–911 m/px would not touch this frame. Neither
is in P4's scope.

The design pane's **4× frame** — the one the primary layer belongs to — is
**24,222** (land 8,442 + borders 748 + fine 5,912 + primary 2,666 + lanes 406 +
ships 6,000 + ring 48); **3.49×** is **24,494** (land 10,462 + borders 760 + fine
6,812 + lanes 412 + ships 6,000 + ring 48) because 287.7 m/px is just below the
new 288 m/px tier-1 switch and draws the 0.001° coast; **2.8×** is **23,074**
(land 7,122 + borders 784 + fine 8,740 + lanes 380 + ships 6,000 + ring 48). The
wide pane's coarse side just below its road switch (1.177×) is **25,146** and its
middle side (1.197×) is **27,365**; at the 1.309×/1.329× shots the middle copy is
**25,877** / **25,884**. The 2,048 pane at 1.539× is **24,310**, at 1.1× is
**27,360**, and at 2.6× is **22,432** (264.6 m/px, now the 0.001° coast).

The ceiling for a 60 Hz frame is 75,000 (`paint.rs`'s `FRAME_TRIANGLE_BUDGET`):
8.58 ms of fixed frame cost plus 107 ns per triangle still fits a 16.65 ms
frame. `the_visible_triangle_budget_is_under_the_frame_budget` sweeps a 1.56:1,
1,600 and 2,048 px wide panes and a tall one, at a 0.1× zoom walk plus the exact
zooms every switch happens at — the three road copies, the short-chain gate and
the three land-tier switches — and at a **7 × 5 grid of camera centres over the
camera's clamp range**, refined by two 9 × 9 rounds around the twenty heaviest
coarse frames. It pins the worst count (**31,815**); that is a grid sample, not an
analytic upper bound. They are model values from that budget, not frame times
measured on a machine.

**Visual acceptance (P4).** The 0.006° tier's worst measured deviation is 1.09 px
at the fitted zoom (see "Worst deviation vs typical displacement" above), which
is over the plan's one-pixel rule by 9% worst-case and typical coastline
displacement is much smaller. The new 0.002° tier is 0.34 px at 1× and 1.197 px
only at its 288 m/px switch. P4 adds three visual risks that have not been checked
on a real window: the 0.3 km feature dropping now removes 3,278 short class-0
chains from the middle and coarse copies, and because the middle copy is
long-only the short chains return only below 371 m/px (about 2.7× on the design
pane) rather than at 1.6×; the coarse copy leaves 2,450 dangling endpoints, many
of them junctions whose connector is gone; and the chain-level 0.004° middle copy
moves a junction by up to 445 m (0.71 px at 1.6×, 0.89 px at 2.0×, 1.20 px at
its 371 m/px switch). The human step is a 1×, a 2.8× and a 4× screenshot of the
strait against the previous build, to be recorded here before P4 is signed off.
Until then this README states the measurement, not a visual pass.

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
