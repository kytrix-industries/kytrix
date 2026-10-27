# `hormuz_land.bin`

The land polygons the Hormuz vessel map draws under its vessels: a clipped
Natural Earth coastline, packed for `include_bytes!`.

## Source and licence

- **Data**: Natural Earth 1:10m land (`ne_10m_land`), 11 features, 446,175
  vertices in the source GeoJSON.
- **URL**:
  <https://raw.githubusercontent.com/nvkelso/natural-earth-vector/master/geojson/ne_10m_land.geojson>
  (10,157,965 bytes).
- **Downloaded**: 2026-10-24.
- **Licence**: **public domain**. Natural Earth's
  [terms of use](https://www.naturalearthdata.com/about/terms-of-use/) place all
  of its raster and vector data in the public domain and state that crediting
  the authors is unnecessary.
- **Clip box**: 48°E–62°E, 21.5°N–28.5°N (roughly the Strait of Hormuz with the
  whole Persian Gulf in view).

The clip leaves **251 rings / 6,266 vertices**. The packed file is 51,144 bytes,
of which 50,128 bytes (48.95 KiB, ≈ 49 KB) are the `f32` longitude/latitude
pairs. Tessellating the rings with the even-odd fill rule yields **5,518
triangles**.

`Scene` tessellates with a tolerance of `5e-6` Mercator radians, not lyon's
default `0.1`. The map is only about 0.13 radians tall, so the default treats
distinct coastline points as touching: it drops 21 of the 6,015 distinct
vertices and yields 5,520 triangles. `5e-6` is about 32 m at the equator (below
one pixel even at the map's maximum 4× zoom), keeps every distinct vertex, and
reproduces the 5,518 measured here. Changing that tolerance changes how lyon
joins nearly touching edges, which is why `src/scene.rs` pins both the constant
and the triangle count.

## Format: KHL1

Little-endian throughout. No padding, no trailing bytes.

```
offset  size            field
0       4               magic "KHL1"
4       4               ring_count: u32
8       4               vert_count: u32
12      …               repeat ring_count times:
                          point_count: u32
                          point_count × (longitude: f32, latitude: f32)
```

- Longitude and latitude are degrees in EPSG:4326.
- Each ring is closed: its last point equals its first point.
- The sum of every `point_count` equals `vert_count`.
- The file length is exactly `12 + 4 * ring_count + 8 * vert_count` bytes.

The rings are drawn as a single path with the **even-odd** fill rule, so lakes
and any sea enclosed by a ring become holes without extra metadata. The
coordinate order in the file is the order the clip produced; nothing sorts or
merges rings afterwards.

## Generation steps

The clip and pack were a one-off step; the script is deliberately not committed
(the asset is the artefact). The pipeline was:

1. **Clip.** Load the GeoJSON, make every feature valid, union all features,
   then intersect with the box `(48, 21.5, 62, 28.5)`. Extract the exterior and
   interior rings of the resulting `Polygon` / `MultiPolygon` in order. This
   gives 251 rings / 6,266 vertices.

   The union matters: the source features share edges along the land they
   split, so clipping each feature on its own and keeping every part leaves
   252 rings / 6,283 vertices instead of the single clipped landmass.

   ```python
   land = unary_union([make_valid(shape(f["geometry"])) for f in document["features"]])
   clipped = land.intersection(box(48.0, 21.5, 62.0, 28.5))
   polygons = [clipped] if clipped.geom_type == "Polygon" else list(clipped.geoms)
   rings = [list(p.exterior.coords) for p in polygons]
   rings += [list(hole.coords) for p in polygons for hole in p.interiors]
   ```

   Used: `shapely` 2.2.0 on GEOS 3.14.1. The resulting f64 rings matched the
   `ne10m_wide.bin` intermediate used while designing the map ring for ring.

2. **f64 → f32.** Convert every longitude and latitude to the nearest `f32`.
   The rounding error is at most 1.9e-6° in longitude and 9.5e-7° in latitude
   (under 0.4 m), far below one pixel at any zoom this map supports.

3. **Pack.** Write the magic, both counts, then each ring's `u32` point count
   followed by its `(lon, lat)` `f32` pairs. Assert the final length is
   `12 + 4 * ring_count + 8 * vert_count` and that no bytes remain.

4. **Verify.** Re-read the file, walk every ring, and check the totals
   (251 rings / 6,266 vertices) and the absence of trailing bytes.
   `Scene::parse` in `src/scene.rs` is what does this at runtime, and its tests
   pin the counts and the 5,518 triangles.

## Runtime

The asset is checked in and embedded:

```rust
const LAND_ASSET: &[u8] = include_bytes!("../assets/hormuz_land.bin");
```

There is no runtime file access and no `rust-embed`. `scene()` parses and
tessellates it once per process into a `OnceLock`, keeps the triangles in Web
Mercator space, and the frame only applies the camera's affine transform.
