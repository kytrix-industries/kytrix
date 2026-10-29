//! The map under the vessels: the packaged OSM base map and the layers it draws.
//!
//! The base map is a checked-in binary ([`MAP_ASSET`]) because it is data, not an
//! icon: one file, read once, embedded by [`include_bytes!`]. It holds six
//! sections — four resolutions of the coastline, the land borders and the road
//! network — in the KHL2 format `assets/README.md` documents.
//!
//! Loading is the only expensive step, and it happens once per process:
//!
//! 1. [`Scene::parse`] walks the header and the six sections, checks every
//!    declared count against the bytes that are actually there, and checks each
//!    line layer's cell table.
//! 2. Each land tier's rings are projected to Web Mercator and tessellated as one
//!    even-odd fill, so lakes and seas cut holes in the land instead of being
//!    filled over. The triangles are then bucketed by centroid into the culling
//!    grid, so a frame emits only the cells its viewport touches; the few that
//!    are too wide for a bucket are kept apart and tested against the viewport.
//!    The four tiers are the same coastline at 111 m, 222 m, 333 m and 1 km, and
//!    a frame draws exactly one of them, chosen by metres per pixel
//!    (`model::land_tier`).
//! 3. The borders and roads are kept as projected polylines cut into pieces, with
//!    the per-cell piece ranges the build wrote.
//!
//! The frame only applies the camera's affine transform to what is visible. It
//! must not tessellate again: rebuilding the path costs several milliseconds while
//! emitting the cached triangles costs a fraction of one.

use std::{ops::Range, sync::OnceLock};

use gpui_kit::{FillOptions, FillRule, PathBuilder, PathStyle, point, px};

use crate::model::{self, CellSpan, GRID_COLS, GRID_ROWS, LAND_TIERS};

/// The packaged base map: KHL2, four land tiers and two line sections.
///
/// See `assets/README.md` for where it came from and how it was packed. A `static`
/// rather than a `const`: a const is inlined at every use, and this one is 1.8 MB —
/// a duplicated copy in rodata would be invisible in the source and paid for in the
/// binary. The tests also compare addresses inside the asset, which only means
/// something if there is exactly one copy.
static MAP_ASSET: &[u8] = include_bytes!("../assets/hormuz_map.bin");

/// The four bytes a KHL2 asset starts with.
const MAGIC: [u8; 4] = *b"KHL2";

/// The only KHL2 version this parser knows.
///
/// Version 1 held one land section, version 2 three and version 3 four. An older
/// asset is not migrated — it is rejected, because the asset is rebuilt from the
/// script in the same commit that changes this number.
const VERSION: u32 = 3;

/// The sections the format defines, in the order they are stored.
const SECTION_COUNT: u32 = 6;

/// `magic | version | bbox | grid_cols | grid_rows | section_count`.
const HEADER_LEN: usize = 36;

/// `kind | point_count | piece_count | cell_count`.
const SECTION_HEADER_LEN: usize = 16;

/// One longitude/latitude pair, each a little-endian `f32`.
const POINT_LEN: usize = 8;

/// `start | len | flags | reserved | length_m`.
const PIECE_LEN: usize = 12;

/// `start | count`.
const CELL_LEN: usize = 8;

/// A land ring needs at least the three points of a triangle.
const MIN_RING_POINTS: usize = 3;

/// How far the stored `f32` bbox may sit from the decimal constants, in degrees.
///
/// The header stores `f32`, so the values read back as the nearest `f32` to
/// `47.802 / 21.443 / 61.875 / 30.042`. Those are the same `f32` values the
/// constants hold, but a comparison against the decimal literals would see the west
/// edge 1.862e-6° off, so the tolerance has to clear that.
const BBOX_EPSILON: f32 = 1e-5;

/// The flags a road piece may carry: bit 0 primary, bit 1 a piece of the middle copy
/// of class 0, bit 2 a piece of the coarse copy, bit 3 a piece of a short chain that
/// is drawn only once a pixel is fine enough. Every other bit is undefined and the
/// parser rejects it.
const ROAD_FLAGS: u8 = 0b1111;

/// The flattening tolerance handed to lyon, in Mercator radians.
///
/// lyon uses this for more than curve flattening: its fill tessellator also treats
/// vertices closer together than half of the tolerance as connected. The value is
/// absolute, and the whole map is only about `0.15` Mercator radians tall, so the
/// default of `0.1` merges distinct coastline points.
///
/// `1e-6` is roughly 6 m at the equator. The asset has land segments as short as
/// 7.2 m (`assets/README.md` counts them), which is longer than the `5e-7` merge
/// distance, so the tessellator never merges two ring points into one. Two of the
/// 23,856 distinct ring points are still absent from the triangles: lyon skips a
/// vertex that sits within the tolerance of the straight line between its
/// neighbours, and both do (by 1.0e-6 and 2.4e-7 rad). That is 6 m at most — a
/// sixtieth of a pixel at four times the fitted zoom — and a test pins both the
/// count and the fact that every skipped point is that close to its neighbours'
/// line. The triangle count is pinned too, because changing the tolerance changes
/// how lyon joins nearly touching edges.
///
/// The four tiers are tessellated at this same tolerance rather than at one
/// scaled to their simplification: a coarser tier's ring points are hundreds of
/// metres apart, so the value only decides how lyon joins a ring against itself,
/// and keeping it fixed keeps tier 0's triangles exactly the ones the map drew
/// before the tiers existed.
const TESSELLATION_TOLERANCE: f32 = 1e-6;

/// Why the packaged base map could not be turned into a [`Scene`].
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum SceneError {
    /// The asset does not start with [`MAGIC`].
    #[error("map asset has an invalid magic")]
    InvalidMagic,
    /// The asset declares a KHL2 version this parser does not know.
    #[error("map asset has an unsupported version")]
    UnsupportedVersion,
    /// The asset ends before a field, a point, a piece or a cell it declares.
    #[error("map asset is truncated")]
    Truncated,
    /// The header's bounding box is not the map this widget draws.
    #[error("map asset has a bounding box that is not the map box")]
    BboxMismatch,
    /// The header's culling grid is not the grid [`visible_cells`] indexes.
    ///
    /// [`visible_cells`]: crate::model::visible_cells
    #[error("map asset has a grid that is not the map grid")]
    GridMismatch,
    /// A section the format requires is absent, out of order, or duplicated.
    #[error("map asset is missing one of its sections")]
    MissingSection,
    /// A piece declares points that are not all inside its section.
    #[error("map asset has a piece whose points are not in its section")]
    PointCountOverflow,
    /// A line layer's cell table is not a partition of its pieces.
    #[error("map asset has an invalid cell table")]
    BadCellRange,
    /// A land section carries a cell table, which the format reserves for line layers.
    #[error("map asset has a land section with a cell table")]
    LandHasCells,
    /// A line layer's pieces are not all covered by its cell table.
    #[error("map asset has a piece that is in no cell")]
    PieceNotInAnyCell,
    /// A piece carries metadata the format does not define for its section.
    #[error("map asset has a piece with metadata the format does not define")]
    BadPiece,
    /// The asset continues after the last section it declares.
    #[error("map asset has trailing bytes")]
    TrailingBytes,
    /// The land tessellated into no triangles at all.
    #[error("map asset has no land")]
    Empty,
    /// A coordinate has no finite Web Mercator position.
    #[error("map asset has a coordinate that is not finite")]
    NonFiniteCoordinate,
    /// lyon rejected the geometry.
    ///
    /// Its error is an untyped `anyhow::Error`, and the rings have already been
    /// validated by the time it runs, so this is a fail-safe rather than a
    /// diagnosis.
    #[error("map asset could not be tessellated")]
    Tessellation,
}

/// The base map, parsed once and kept in Web Mercator space.
pub(super) struct Scene {
    /// The coastline at each tier, coarsest last: index 0 is 0.001°, index 3 is
    /// 0.006°. A frame draws exactly one of them — see [`Self::land`].
    pub land: [LandLayer; LAND_TIERS],
    /// The land borders, cut into per-cell pieces by the build.
    pub borders: LineLayer,
    /// The road network, cut into per-cell pieces by the build.
    pub roads: LineLayer,
}

/// The land as triangles, bucketed into the culling grid.
pub(super) struct LandLayer {
    /// Triangle rows: three `x, y` pairs each, in Mercator radians.
    ///
    /// Only the triangles the grid can bucket by centroid; see [`Self::wide`].
    pub triangles: Vec<[f32; 6]>,
    /// The triangles of each cell, in row-major cell order.
    pub cells: Vec<Range<u32>>,
    /// The triangles too wide to bucket by centroid.
    ///
    /// A frame visits the cells its viewport touches plus one cell of margin, which
    /// covers every triangle whose box lies within one cell of its centroid cell. The
    /// tessellator also emits triangles spanning most of the map — the largest covers
    /// 23.4% of it, and these wide ones are 98.8% of the land's area — so bucketing those
    /// by centroid would leave them out of nearly every frame that can see them. They
    /// are kept here instead and tested against the viewport's own rectangle: 3,389
    /// boxes per frame at the finest tier rather than holes in the coastline.
    pub wide: Vec<[f32; 6]>,
}

/// One line layer: projected polylines, their pieces, and the per-cell ranges.
pub(super) struct LineLayer {
    /// The section's points, projected to Mercator radians.
    pub points: Vec<[f32; 2]>,
    /// The polylines, each a range of `points`.
    pub pieces: Vec<Piece>,
    /// The pieces of each cell, in row-major cell order.
    pub cells: Vec<Range<u32>>,
}

/// One polyline of a [`LineLayer`].
pub(super) struct Piece {
    /// The first point of the piece, as an index into its layer's points.
    pub start: u32,
    /// How many points the piece has.
    ///
    /// The format stores this as a `u16`, which is the build's 65,535-point bound: a
    /// piece can never declare more points than the field can hold.
    pub len: u16,
    /// Bit 0 primary, bit 1 a piece of the middle copy of the motorways and trunks,
    /// bit 2 a piece of the coarse copy, bit 3 a piece of a short class-0 chain; zero
    /// on borders. Bits the format does not define are rejected.
    pub flags: u8,
    /// The piece's length in metres, as the build measured it; zero on borders.
    pub length_m: u32,
}

impl Scene {
    /// Parses a KHL2 asset.
    ///
    /// The header and every section's declared size are checked before anything is
    /// projected or tessellated, and the line layers' pieces and cell tables are
    /// checked before the land is handed to lyon: a malformed asset never reaches the
    /// tessellator. The land tiers are the only layers that have to be built after
    /// those checks, so they are filled in last.
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, SceneError> {
        parse_header(bytes)?;

        // The sections are read in the order the format stores them, and
        // `RawSection::parse` rejects a kind that is not the one this position
        // declares: a file whose sections are reordered, duplicated or missing fails
        // here rather than being read as a different map.
        let mut offset = HEADER_LEN;
        let land_0 = RawSection::parse(bytes, &mut offset, SectionKind::LandFinest)?;
        let land_1 = RawSection::parse(bytes, &mut offset, SectionKind::LandFine)?;
        let land_2 = RawSection::parse(bytes, &mut offset, SectionKind::LandMiddle)?;
        let land_3 = RawSection::parse(bytes, &mut offset, SectionKind::LandCoarse)?;
        let borders = RawSection::parse(bytes, &mut offset, SectionKind::Borders)?;
        let roads = RawSection::parse(bytes, &mut offset, SectionKind::Roads)?;
        if offset != bytes.len() {
            return Err(SceneError::TrailingBytes);
        }

        // The line layers are checked before the land is tessellated: a malformed cell
        // table is a cheaper failure than 61,000 ring points through lyon, and the
        // four tiers are the most expensive thing this parser does.
        let borders = LineLayer::parse(&borders)?;
        let roads = LineLayer::parse(&roads)?;
        borders.validate(LineKind::Borders)?;
        roads.validate(LineKind::Roads)?;

        Ok(Self {
            land: [
                LandLayer::parse(&land_0)?,
                LandLayer::parse(&land_1)?,
                LandLayer::parse(&land_2)?,
                LandLayer::parse(&land_3)?,
            ],
            borders,
            roads,
        })
    }

    /// The coastline a projection whose pixels are `metres_per_pixel` wide draws.
    ///
    /// The tier is a property of the projection's scale, not of the zoom number: a
    /// zoom is relative to the pane's own fitted scale, which changes with its size.
    pub(super) fn land(&self, metres_per_pixel: f32) -> &LandLayer {
        &self.land[model::land_tier(metres_per_pixel)]
    }
}

impl LandLayer {
    /// Every triangle that can reach a viewport covering `visible`.
    ///
    /// `cells` is the viewport's cells grown by one, which is what covers the triangles
    /// bucketed by centroid; [`Self::wide`] is tested against the viewport itself. Each
    /// triangle is yielded at most once, so the caller can draw the path directly.
    pub(super) fn visible(
        &self,
        cells: CellSpan,
        visible: [f32; 4],
    ) -> impl Iterator<Item = &[f32; 6]> {
        let celled = cells.indices().flat_map(move |cell| {
            let range = &self.cells[cell as usize];
            self.triangles[range.start as usize..range.end as usize].iter()
        });
        celled.chain(self.wide.iter().filter(move |triangle| {
            let [west, south, east, north] = triangle_bounds(triangle);
            west <= visible[2] && east >= visible[0] && south <= visible[3] && north >= visible[1]
        }))
    }

    /// Tessellates the land rings and buckets the triangles by centroid.
    ///
    /// The rings are one even-odd fill: a lake or a sea is a ring inside another and
    /// cuts a hole rather than being filled over. Every ring's points are checked to
    /// lie inside the section before they are read, and every projected vertex is
    /// checked to be finite before lyon sees it. A land section carries no cell table
    /// — the grid is made at run time from the triangles — so a section that declares
    /// one is rejected rather than silently ignored.
    fn parse(section: &RawSection<'_>) -> Result<Self, SceneError> {
        if !section.cells.is_empty() {
            return Err(SceneError::LandHasCells);
        }
        let options = FillOptions::default()
            .with_fill_rule(FillRule::EvenOdd)
            .with_tolerance(TESSELLATION_TOLERANCE);
        let mut builder = PathBuilder::fill().with_style(PathStyle::Fill(options));
        let point_count = section.points.len() / POINT_LEN;

        for piece in section.pieces.as_chunks::<PIECE_LEN>().0 {
            let start = read_u32(piece, 0) as usize;
            let len = usize::from(read_u16(piece, 4));
            if piece[6] != 0 || piece[7] != 0 || read_u32(piece, 8) != 0 {
                return Err(SceneError::BadPiece);
            }
            if len < MIN_RING_POINTS || start > point_count || len > point_count - start {
                return Err(SceneError::PointCountOverflow);
            }

            for index in start..start + len {
                let [longitude, latitude] = read_point(section.points, index * POINT_LEN);
                let [x, y] = model::project(longitude, latitude);
                if !x.is_finite() || !y.is_finite() {
                    return Err(SceneError::NonFiniteCoordinate);
                }
                let at = point(px(x), px(y));
                if index == start {
                    builder.move_to(at);
                } else {
                    builder.line_to(at);
                }
            }
            builder.close();
        }

        let path = builder.build().map_err(|_| SceneError::Tessellation)?;
        let mut triangles = Vec::with_capacity(path.vertices.len() / 3);
        for vertices in path.vertices.as_chunks::<3>().0 {
            triangles.push([
                f32::from(vertices[0].xy_position.x),
                f32::from(vertices[0].xy_position.y),
                f32::from(vertices[1].xy_position.x),
                f32::from(vertices[1].xy_position.y),
                f32::from(vertices[2].xy_position.x),
                f32::from(vertices[2].xy_position.y),
            ]);
        }
        if triangles.is_empty() {
            return Err(SceneError::Empty);
        }

        let (triangles, cells, wide) = bucket(triangles);
        Ok(Self {
            triangles,
            cells,
            wide,
        })
    }
}

impl LineLayer {
    /// Projects a line section's points and keeps its pieces and cell table.
    ///
    /// The declared counts were already checked against the bytes by
    /// [`RawSection::parse`], so every slice here is exactly the size its header says.
    fn parse(section: &RawSection<'_>) -> Result<Self, SceneError> {
        let mut points = Vec::with_capacity(section.points.len() / POINT_LEN);
        for chunk in section.points.as_chunks::<POINT_LEN>().0 {
            let [longitude, latitude] = read_point(chunk, 0);
            let [x, y] = model::project(longitude, latitude);
            if !x.is_finite() || !y.is_finite() {
                return Err(SceneError::NonFiniteCoordinate);
            }
            points.push([x, y]);
        }

        let mut pieces = Vec::with_capacity(section.pieces.len() / PIECE_LEN);
        for chunk in section.pieces.as_chunks::<PIECE_LEN>().0 {
            if chunk[7] != 0 {
                return Err(SceneError::BadPiece);
            }
            pieces.push(Piece {
                start: read_u32(chunk, 0),
                len: read_u16(chunk, 4),
                flags: chunk[6],
                length_m: read_u32(chunk, 8),
            });
        }

        let mut cells = Vec::with_capacity(section.cells.len() / CELL_LEN);
        for chunk in section.cells.as_chunks::<CELL_LEN>().0 {
            let start = read_u32(chunk, 0);
            let count = read_u32(chunk, 4);
            let end = start.checked_add(count).ok_or(SceneError::BadCellRange)?;
            cells.push(start..end);
        }

        Ok(Self {
            points,
            pieces,
            cells,
        })
    }

    /// Checks that the pieces fit the points and that the cells partition the pieces.
    ///
    /// The cell table is `GRID_COLS * GRID_ROWS` consecutive ranges that start at the
    /// first piece, leave no gap and stop at the last piece: every piece therefore
    /// belongs to exactly one cell. A section without a cell table (the land) is
    /// never built as one of these; its grid is made at run time from the triangles.
    fn validate(&self, kind: LineKind) -> Result<(), SceneError> {
        if self.cells.len() != (GRID_COLS * GRID_ROWS) as usize {
            return Err(SceneError::BadCellRange);
        }

        let mut next = 0;
        for range in &self.cells {
            if range.start != next || range.end > self.pieces.len() as u32 {
                return Err(SceneError::BadCellRange);
            }
            next = range.end;
        }
        if next != self.pieces.len() as u32 {
            return Err(SceneError::PieceNotInAnyCell);
        }

        for piece in &self.pieces {
            let start = piece.start as usize;
            let end = start
                .checked_add(usize::from(piece.len))
                .ok_or(SceneError::PointCountOverflow)?;
            if end > self.points.len() {
                return Err(SceneError::PointCountOverflow);
            }

            let metadata_is_defined = match kind {
                LineKind::Borders => piece.flags == 0 && piece.length_m == 0,
                LineKind::Roads => piece.flags & !ROAD_FLAGS == 0,
            };
            if !metadata_is_defined {
                return Err(SceneError::BadPiece);
            }
        }

        Ok(())
    }
}

/// The one [`Scene`] of this process, parsed on first use.
///
/// A failure is cached too: the asset is compiled in, so retrying would walk the same
/// bytes and reach the same error. Callers get a clone of it.
pub(super) fn scene() -> Result<&'static Scene, SceneError> {
    static SCENE: OnceLock<Result<Scene, SceneError>> = OnceLock::new();
    SCENE
        .get_or_init(|| Scene::parse(MAP_ASSET))
        .as_ref()
        .map_err(Clone::clone)
}

/// The sections a KHL2 asset stores, in the order it stores them.
///
/// The four land tiers come first, coarsest last: the parser tessellates each into
/// its own [`LandLayer`], and a frame draws the one [`model::land_tier`] picks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SectionKind {
    /// Closed coastline rings at 0.001°, tessellated at run time.
    LandFinest = 1,
    /// Closed coastline rings at 0.002°, tessellated at run time.
    LandFine = 2,
    /// Closed coastline rings at 0.003°, tessellated at run time.
    LandMiddle = 3,
    /// Closed coastline rings at 0.006°, tessellated at run time.
    LandCoarse = 4,
    /// Land borders, cut into per-cell pieces by the build.
    Borders = 5,
    /// Roads, cut into per-cell pieces by the build.
    Roads = 6,
}

impl SectionKind {
    /// The kind one stored `u32` names, or `None` when it names no section.
    fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::LandFinest),
            2 => Some(Self::LandFine),
            3 => Some(Self::LandMiddle),
            4 => Some(Self::LandCoarse),
            5 => Some(Self::Borders),
            6 => Some(Self::Roads),
            _ => None,
        }
    }
}

/// What a line layer's pieces carry, which decides what their metadata may say.
enum LineKind {
    /// Land borders: no flags and no length.
    Borders,
    /// Roads: the three flag bits (0, 2, 3), and a length in metres.
    Roads,
}

/// One section's header and the three byte ranges it declares.
///
/// The slices are exactly the sizes the header declares, so every reader below can
/// index them without a second bounds check.
struct RawSection<'a> {
    /// `point_count × (longitude f32, latitude f32)`.
    points: &'a [u8],
    /// `piece_count × (start u32, len u16, flags u8, reserved u8, length_m u32)`.
    pieces: &'a [u8],
    /// `cell_count × (start u32, count u32)`.
    cells: &'a [u8],
}

impl<'a> RawSection<'a> {
    /// Reads one section's header and cuts its three ranges out of `bytes`.
    ///
    /// The section's kind must be the one this position of the file declares, and
    /// every declared count must fit in the bytes that are left: a count that runs
    /// past the end is a truncated asset, not a section with missing data.
    fn parse(bytes: &'a [u8], offset: &mut usize, kind: SectionKind) -> Result<Self, SceneError> {
        let header = take(bytes, offset, SECTION_HEADER_LEN)?;
        if SectionKind::from_u32(read_u32(header, 0)) != Some(kind) {
            return Err(SceneError::MissingSection);
        }

        let points = read_u32(header, 4) as usize;
        let pieces = read_u32(header, 8) as usize;
        let cells = read_u32(header, 12) as usize;
        Ok(Self {
            points: take(
                bytes,
                offset,
                points.checked_mul(POINT_LEN).ok_or(SceneError::Truncated)?,
            )?,
            pieces: take(
                bytes,
                offset,
                pieces.checked_mul(PIECE_LEN).ok_or(SceneError::Truncated)?,
            )?,
            cells: take(
                bytes,
                offset,
                cells.checked_mul(CELL_LEN).ok_or(SceneError::Truncated)?,
            )?,
        })
    }
}

/// Checks the header: magic, version, bounding box, grid and section count.
fn parse_header(bytes: &[u8]) -> Result<(), SceneError> {
    if bytes.len() < MAGIC.len() {
        return Err(SceneError::Truncated);
    }
    if bytes[..MAGIC.len()] != MAGIC {
        return Err(SceneError::InvalidMagic);
    }
    if bytes.len() < HEADER_LEN {
        return Err(SceneError::Truncated);
    }
    if read_u32(bytes, 4) != VERSION {
        return Err(SceneError::UnsupportedVersion);
    }

    let stored = [
        read_f32(bytes, 8),
        read_f32(bytes, 12),
        read_f32(bytes, 16),
        read_f32(bytes, 20),
    ];
    let expected = [
        model::MAP_WEST,
        model::MAP_SOUTH,
        model::MAP_EAST,
        model::MAP_NORTH,
    ];
    for (stored, expected) in stored.into_iter().zip(expected) {
        if !stored.is_finite() || (stored - expected).abs() > BBOX_EPSILON {
            return Err(SceneError::BboxMismatch);
        }
    }

    if read_u32(bytes, 24) != GRID_COLS || read_u32(bytes, 28) != GRID_ROWS {
        return Err(SceneError::GridMismatch);
    }
    if read_u32(bytes, 32) != SECTION_COUNT {
        return Err(SceneError::MissingSection);
    }
    Ok(())
}

/// Cuts `len` bytes off `bytes` at `offset`, or reports that they are not there.
fn take<'a>(bytes: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], SceneError> {
    let end = offset.checked_add(len).ok_or(SceneError::Truncated)?;
    let slice = bytes.get(*offset..end).ok_or(SceneError::Truncated)?;
    *offset = end;
    Ok(slice)
}

/// Reorders the triangles into the culling grid, by centroid.
///
/// The result is one array in cell order plus the range every cell occupies in it: a
/// frame indexes the ranges of the cells its viewport touches and emits nothing else.
/// A cell no triangle's centroid falls in keeps an empty range. The triangles too wide
/// for that bucket are handed back separately.
fn bucket(triangles: Vec<[f32; 6]>) -> (Vec<[f32; 6]>, Vec<Range<u32>>, Vec<[f32; 6]>) {
    let mut buckets: Vec<Vec<[f32; 6]>> = vec![Vec::new(); (GRID_COLS * GRID_ROWS) as usize];
    let mut wide = Vec::new();
    for triangle in triangles {
        match cell_of(&triangle) {
            Some(cell) => buckets[cell as usize].push(triangle),
            None => wide.push(triangle),
        }
    }

    let mut sorted = Vec::with_capacity(buckets.iter().map(Vec::len).sum());
    let mut cells = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        let start = sorted.len() as u32;
        sorted.extend(bucket);
        cells.push(start..sorted.len() as u32);
    }
    (sorted, cells, wide)
}

/// The cell a triangle's centroid falls in, or `None` when the triangle is too wide.
///
/// A frame visits the cells its viewport touches plus one cell on every side, so a
/// triangle bucketed here is certain to be drawn only when its centroid cell is at most
/// one cell from every cell its box touches: that is, when the box spans at most two
/// cells on each axis. Everything wider goes to [`LandLayer::wide`] instead.
fn cell_of(triangle: &[f32; 6]) -> Option<u32> {
    let [west, _, east, _] = model::mercator_bounds();
    let [min_x, min_y, max_x, max_y] = triangle_bounds(triangle);
    let first_column = column_of(min_x, west, east);
    let last_column = column_of(max_x, west, east);
    let first_row = row_of(min_y);
    let last_row = row_of(max_y);
    if last_column - first_column > 1 || last_row - first_row > 1 {
        return None;
    }

    let column = column_of((triangle[0] + triangle[2] + triangle[4]) / 3.0, west, east);
    let row = row_of((triangle[1] + triangle[3] + triangle[5]) / 3.0);
    Some(row * GRID_COLS + column)
}

/// The `[west, south, east, north]` box a triangle covers, in Mercator radians.
fn triangle_bounds(triangle: &[f32; 6]) -> [f32; 4] {
    [
        triangle[0].min(triangle[2]).min(triangle[4]),
        triangle[1].min(triangle[3]).min(triangle[5]),
        triangle[0].max(triangle[2]).max(triangle[4]),
        triangle[1].max(triangle[3]).max(triangle[5]),
    ]
}

/// The column a Mercator `x` falls in.
///
/// A coordinate on the map box's east edge would land one past the last column, so the
/// index is clamped: the edge belongs to the edge cell.
fn column_of(x: f32, west: f32, east: f32) -> u32 {
    ((x - west) / (east - west) * GRID_COLS as f32)
        .floor()
        .clamp(0.0, GRID_COLS as f32 - 1.0) as u32
}

/// The row a Mercator `y` falls in.
///
/// The rows are latitude bands, not Mercator bands: the build cut the asset's cells in
/// degrees, so the row is read from the point's latitude. Reading it off the Mercator
/// `y` would put a point up to a quarter of a row away from the cell the build wrote.
fn row_of(y: f32) -> u32 {
    let latitude = model::latitude_of(y);
    ((latitude - model::MAP_SOUTH) / (model::MAP_NORTH - model::MAP_SOUTH) * GRID_ROWS as f32)
        .floor()
        .clamp(0.0, GRID_ROWS as f32 - 1.0) as u32
}

/// Reads a little-endian `u32`; the caller has a slice that holds it.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(read_array(bytes, offset))
}

/// Reads a little-endian `u16`; the caller has a slice that holds it.
fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

/// Reads a little-endian `f32`; the caller has a slice that holds it.
fn read_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(read_array(bytes, offset))
}

/// Reads one `longitude, latitude` pair; the caller has a slice that holds it.
fn read_point(bytes: &[u8], offset: usize) -> [f32; 2] {
    [read_f32(bytes, offset), read_f32(bytes, offset + 4)]
}

/// Reads four bytes as a fixed-size array; the caller checked the length.
fn read_array(bytes: &[u8], offset: usize) -> [u8; 4] {
    [
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The land's stored points, tier by tier, as the checked-in asset declares them.
    const LAND_POINTS: [u32; LAND_TIERS] = [25_446, 15_767, 12_113, 8_099];
    /// The land rings, tier by tier.
    const LAND_PIECES: [u32; LAND_TIERS] = [854, 805, 790, 785];
    /// The border points, as the checked-in asset declares them.
    const BORDER_POINTS: u32 = 1_630;
    /// The border pieces, as the checked-in asset declares them.
    const BORDER_PIECES: u32 = 201;
    /// The road points, as the checked-in asset declares them.
    const ROAD_POINTS: u32 = 60_240;
    /// The road pieces, as the checked-in asset declares them.
    const ROAD_PIECES: u32 = 21_748;
    /// The cells each line layer's table holds.
    const CELLS: u32 = GRID_COLS * GRID_ROWS;

    /// The triangles each land tier tessellates into at [`TESSELLATION_TOLERANCE`].
    ///
    /// The 0.001° tier is the count the map drew before the tiers existed and is
    /// pinned to the triangle; the coarser ones are pinned to the same standard,
    /// because they are the whole point of the change — the fitted zoom draws 5,744
    /// triangles, not 22,876. The build's own estimate (`points - 3 × rings`) reads
    /// 22,884 / 13,352 / 9,743 / 5,744: lyon skips the ring points that are collinear
    /// with their neighbours, and a simplified ring has a few of those.
    const LAND_TRIANGLES: [usize; LAND_TIERS] = [22_876, 13_347, 9_736, 5_744];
    /// The triangles of each tier the culling grid can bucket by centroid.
    const LAND_CELL_TRIANGLES: [usize; LAND_TIERS] = [19_487, 11_277, 8_145, 4_786];
    /// The triangles of each tier too wide to bucket by centroid.
    const LAND_WIDE_TRIANGLES: [usize; LAND_TIERS] = [3_389, 2_070, 1_591, 958];
    /// The distinct ring points of tier 0: 25,446 stored points less one closing point
    /// per ring.
    const LAND_RING_POINTS: usize = 24_592;
    /// The distinct points the tier 0 triangles are built from.
    ///
    /// Two ring points are not in them: lyon skips a vertex that sits within the
    /// tessellation tolerance of the straight line between its neighbours, which both
    /// of them do (see `no_ring_point_is_merged_away_by_the_tessellator`).
    const LAND_TRIANGLE_POINTS: usize = 24_590;
    /// How many ring points the tessellator skipped for that reason.
    const LAND_SKIPPED_POINTS: usize = LAND_RING_POINTS - LAND_TRIANGLE_POINTS;

    /// How far a Mercator coordinate may sit from the map box, in radians.
    ///
    /// The stored points are `f32` degrees, so the extreme ones can sit 1.862e-6°
    /// (3.25e-8 rad) outside the decimal constants; 1e-6 rad is about 6 m.
    const MERCATOR_EPSILON: f32 = 1e-6;

    /// How far a stored point may sit outside its cell's box, in degrees.
    ///
    /// The build cuts cells in `f64` and stores the nearest `f32`, so a piece can sit
    /// 1.892e-6° outside its cell; the containment check needs more than that.
    const DEGREES_EPSILON: f64 = 2e-6;

    /// The map box the build cut the cells with, in `f64` degrees.
    ///
    /// The build's edges come from the decimal constants, not from the `f32` header:
    /// deriving them from the header's west edge would move the westmost column by
    /// 1.862e-6° and read as an overshoot twice as large as it is.
    const GRID_BOX: [f64; 4] = [47.802, 21.443, 61.875, 30.042];

    /// One section a fixture asset is packed from.
    #[derive(Default)]
    struct SectionSpec {
        kind: u32,
        points: Vec<[f32; 2]>,
        /// `start`, `len`, `flags`, `length_m`.
        pieces: Vec<(u32, u16, u8, u32)>,
        /// `start`, `count`.
        cells: Vec<(u32, u32)>,
        /// The byte every piece writes as its reserved byte.
        reserved: u8,
    }

    fn land_section(kind: SectionKind, points: &[[f32; 2]], pieces: &[(u32, u16)]) -> SectionSpec {
        SectionSpec {
            kind: kind as u32,
            points: points.to_vec(),
            pieces: pieces
                .iter()
                .map(|&(start, len)| (start, len, 0, 0))
                .collect(),
            ..SectionSpec::default()
        }
    }

    /// The same rings at all four tiers, which is what a fixture needs: the parser
    /// tessellates each section on its own, and a fixture's rings do not have four
    /// resolutions.
    fn land_tiers(points: &[[f32; 2]], pieces: &[(u32, u16)]) -> [SectionSpec; LAND_TIERS] {
        [
            land_section(SectionKind::LandFinest, points, pieces),
            land_section(SectionKind::LandFine, points, pieces),
            land_section(SectionKind::LandMiddle, points, pieces),
            land_section(SectionKind::LandCoarse, points, pieces),
        ]
    }

    fn line_section(
        kind: SectionKind,
        points: &[[f32; 2]],
        pieces: &[(u32, u16, u8, u32)],
        cells: Vec<(u32, u32)>,
    ) -> SectionSpec {
        SectionSpec {
            kind: kind as u32,
            points: points.to_vec(),
            pieces: pieces.to_vec(),
            cells,
            ..SectionSpec::default()
        }
    }

    /// A cell table with every piece in cell 0 and every other cell empty.
    fn one_cell(pieces: u32) -> Vec<(u32, u32)> {
        let mut cells = vec![(pieces, 0); CELLS as usize];
        cells[0] = (0, pieces);
        cells
    }

    fn empty_line_section(kind: SectionKind) -> SectionSpec {
        line_section(kind, &[], &[], vec![(0, 0); CELLS as usize])
    }

    /// Packs a header and the six sections into a KHL2 asset.
    fn asset(land: [SectionSpec; LAND_TIERS], borders: SectionSpec, roads: SectionSpec) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        for edge in [
            model::MAP_WEST,
            model::MAP_SOUTH,
            model::MAP_EAST,
            model::MAP_NORTH,
        ] {
            bytes.extend_from_slice(&edge.to_le_bytes());
        }
        bytes.extend_from_slice(&GRID_COLS.to_le_bytes());
        bytes.extend_from_slice(&GRID_ROWS.to_le_bytes());
        bytes.extend_from_slice(&SECTION_COUNT.to_le_bytes());

        for section in land.into_iter().chain([borders, roads]) {
            bytes.extend_from_slice(&section.kind.to_le_bytes());
            bytes.extend_from_slice(&(section.points.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(section.pieces.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(section.cells.len() as u32).to_le_bytes());
            for [longitude, latitude] in section.points {
                bytes.extend_from_slice(&longitude.to_le_bytes());
                bytes.extend_from_slice(&latitude.to_le_bytes());
            }
            for (start, len, flags, length_m) in section.pieces {
                bytes.extend_from_slice(&start.to_le_bytes());
                bytes.extend_from_slice(&len.to_le_bytes());
                bytes.push(flags);
                bytes.push(section.reserved);
                bytes.extend_from_slice(&length_m.to_le_bytes());
            }
            for (start, count) in section.cells {
                bytes.extend_from_slice(&start.to_le_bytes());
                bytes.extend_from_slice(&count.to_le_bytes());
            }
        }
        bytes
    }

    /// A valid land tier: one closed triangle.
    fn land_triangle() -> [SectionSpec; LAND_TIERS] {
        land_tiers(
            &[[50.0, 25.0], [51.0, 25.0], [50.0, 26.0], [50.0, 25.0]],
            &[(0, 4)],
        )
    }

    /// A minimal valid asset: one closed land ring at every tier, and two empty line layers.
    fn ring_asset(points: &[[f32; 2]]) -> Vec<u8> {
        asset(
            land_tiers(points, &[(0, points.len() as u16)]),
            empty_line_section(SectionKind::Borders),
            empty_line_section(SectionKind::Roads),
        )
    }

    /// A minimal valid asset: one triangle at every tier, closed.
    fn triangle_asset() -> Vec<u8> {
        asset(
            land_triangle(),
            empty_line_section(SectionKind::Borders),
            empty_line_section(SectionKind::Roads),
        )
    }

    /// A valid asset with every kind of section filled: one triangle of land at every
    /// tier, two border pieces and two road pieces, so the fuzz reaches the cell table
    /// and the piece metadata as well as the land.
    fn line_asset() -> Vec<u8> {
        let borders = line_section(
            SectionKind::Borders,
            &[[50.0, 25.0], [51.0, 25.0], [52.0, 25.0]],
            &[(0, 2, 0, 0), (2, 1, 0, 0)],
            one_cell(2),
        );
        let roads = line_section(
            SectionKind::Roads,
            &[[50.0, 25.0], [51.0, 25.0], [52.0, 25.0]],
            &[(0, 2, 0, 1_000), (2, 1, 1, 500)],
            one_cell(2),
        );
        asset(land_triangle(), borders, roads)
    }

    /// The sections the checked-in asset stores, in the order the file stores them.
    const ALL: [SectionKind; SECTION_COUNT as usize] = [
        SectionKind::LandFinest,
        SectionKind::LandFine,
        SectionKind::LandMiddle,
        SectionKind::LandCoarse,
        SectionKind::Borders,
        SectionKind::Roads,
    ];

    /// The six raw sections of the checked-in asset, in the file's order.
    fn raw_sections() -> [RawSection<'static>; SECTION_COUNT as usize] {
        let mut offset = HEADER_LEN;
        ALL.map(|kind| {
            RawSection::parse(MAP_ASSET, &mut offset, kind)
                .unwrap_or_else(|error| panic!("{kind:?} section: {error}"))
        })
    }

    /// The extent the triangles cover, as `[min, max]` in Mercator radians.
    ///
    /// The scene does not keep it: the frame draws every triangle against the map box
    /// rather than against the land's own extent, so the extent is only ever a fixture
    /// check — computed here from the triangles it describes.
    fn land_extent(land: &LandLayer) -> [[f32; 2]; 2] {
        let mut extent = [[f32::INFINITY; 2], [f32::NEG_INFINITY; 2]];
        for triangle in land.triangles.iter().chain(land.wide.iter()) {
            for pair in triangle.as_chunks::<2>().0 {
                let [x, y] = *pair;
                extent[0][0] = extent[0][0].min(x);
                extent[0][1] = extent[0][1].min(y);
                extent[1][0] = extent[1][0].max(x);
                extent[1][1] = extent[1][1].max(y);
            }
        }
        extent
    }

    /// The degree box of one cell, in `[west, south, east, north]` order.
    fn cell_box(index: u32) -> [f64; 4] {
        let [west, south, east, north] = GRID_BOX;
        let column = f64::from(index % GRID_COLS);
        let row = f64::from(index / GRID_COLS);
        [
            west + (east - west) * column / f64::from(GRID_COLS),
            south + (north - south) * row / f64::from(GRID_ROWS),
            west + (east - west) * (column + 1.0) / f64::from(GRID_COLS),
            south + (north - south) * (row + 1.0) / f64::from(GRID_ROWS),
        ]
    }

    #[test]
    fn the_asset_declares_the_measured_shape() {
        assert_eq!(&MAP_ASSET[..MAGIC.len()], &MAGIC);
        assert_eq!(read_u32(MAP_ASSET, 4), VERSION);
        assert_eq!(read_u32(MAP_ASSET, 24), GRID_COLS);
        assert_eq!(read_u32(MAP_ASSET, 28), GRID_ROWS);
        assert_eq!(read_u32(MAP_ASSET, 32), SECTION_COUNT);

        let raw = raw_sections();
        for (tier, section) in raw[..LAND_TIERS].iter().enumerate() {
            assert_eq!(section.points.len() / POINT_LEN, LAND_POINTS[tier] as usize);
            assert_eq!(section.pieces.len() / PIECE_LEN, LAND_PIECES[tier] as usize);
            assert_eq!(section.cells.len(), 0, "the land is binned at run time");
        }
        assert_eq!(raw[4].points.len() / POINT_LEN, BORDER_POINTS as usize);
        assert_eq!(raw[4].pieces.len() / PIECE_LEN, BORDER_PIECES as usize);
        assert_eq!(raw[4].cells.len() / CELL_LEN, CELLS as usize);
        assert_eq!(raw[5].points.len() / POINT_LEN, ROAD_POINTS as usize);
        assert_eq!(raw[5].pieces.len() / PIECE_LEN, ROAD_PIECES as usize);
        assert_eq!(raw[5].cells.len() / CELL_LEN, CELLS as usize);

        let scene = scene().expect("the checked-in asset parses");
        for (tier, land) in scene.land.iter().enumerate() {
            assert_eq!(
                land.triangles.len(),
                LAND_CELL_TRIANGLES[tier],
                "tier {tier}"
            );
            assert_eq!(land.wide.len(), LAND_WIDE_TRIANGLES[tier], "tier {tier}");
            assert_eq!(
                land.triangles.len() + land.wide.len(),
                LAND_TRIANGLES[tier],
                "every triangle of tier {tier} is either bucketed or kept for the viewport test",
            );
            assert_eq!(land.cells.len(), CELLS as usize);
        }
        assert_eq!(scene.borders.points.len(), BORDER_POINTS as usize);
        assert_eq!(scene.borders.pieces.len(), BORDER_PIECES as usize);
        assert_eq!(scene.borders.cells.len(), CELLS as usize);
        assert_eq!(scene.roads.points.len(), ROAD_POINTS as usize);
        assert_eq!(scene.roads.pieces.len(), ROAD_PIECES as usize);
        assert_eq!(scene.roads.cells.len(), CELLS as usize);
    }

    /// Each tier is a coarser coast than the one before it: fewer rings, fewer points
    /// and fewer triangles, in that order. This is what makes the tier choice worth
    /// anything, so it is pinned to the asset's own counts rather than to a tolerance.
    #[test]
    fn the_land_tiers_get_strictly_cheaper() {
        let scene = scene().expect("the checked-in asset parses");
        for pair in LAND_TRIANGLES.windows(2) {
            assert!(
                pair[0] > pair[1],
                "the tiers are not ordered coarse-last: {LAND_TRIANGLES:?}",
            );
        }
        for pair in LAND_PIECES.windows(2) {
            assert!(
                pair[0] > pair[1],
                "a coarser tier has at least as many rings: {LAND_PIECES:?}",
            );
        }
        for pair in LAND_POINTS.windows(2) {
            assert!(
                pair[0] > pair[1],
                "a coarser tier has at least as many points: {LAND_POINTS:?}",
            );
        }
        assert_eq!(
            scene
                .land
                .iter()
                .map(|land| land.triangles.len() + land.wide.len())
                .collect::<Vec<_>>(),
            LAND_TRIANGLES,
            "the parsed layers hold the counts this module pins",
        );
        // The fitted zoom draws the coarsest tier, and it costs less than a third of the
        // finest one: that ratio is the whole of the P1 saving.
        assert!(
            LAND_TRIANGLES[LAND_TIERS - 1] * 3 < LAND_TRIANGLES[0],
            "the coarsest tier is not the cheap one: {LAND_TRIANGLES:?}",
        );
    }

    /// Every triangle of every tier, the bucketed ones and the wide ones.
    fn all_land_triangles(scene: &Scene) -> impl Iterator<Item = &[f32; 6]> {
        scene
            .land
            .iter()
            .flat_map(|land| land.triangles.iter().chain(land.wide.iter()))
    }

    #[test]
    fn the_land_covers_the_map_box_and_no_more() {
        let scene = scene().expect("the checked-in asset parses");
        let [west, south, east, north] = model::mercator_bounds();
        for (tier, land) in scene.land.iter().enumerate() {
            let [min, max] = land_extent(land);
            assert!(
                (min[0] - west).abs() <= MERCATOR_EPSILON,
                "tier {tier} starts {min:?} from the west edge",
            );
            assert!((min[1] - south).abs() <= MERCATOR_EPSILON, "tier {tier}");
            assert!((max[0] - east).abs() <= MERCATOR_EPSILON, "tier {tier}");
            assert!((max[1] - north).abs() <= MERCATOR_EPSILON, "tier {tier}");
        }
    }

    #[test]
    fn every_land_triangle_is_finite_and_inside_the_map_box() {
        // Both halves, at every tier: the wide triangles are the ones the culling tests
        // against the viewport, and a coordinate outside the box there is a triangle
        // drawn in the wrong place rather than a missing one.
        let scene = scene().expect("the checked-in asset parses");
        let [west, south, east, north] = model::mercator_bounds();
        for triangle in all_land_triangles(scene) {
            for pair in triangle.as_chunks::<2>().0 {
                let [x, y] = *pair;
                assert!(x.is_finite() && y.is_finite());
                assert!(x >= west - MERCATOR_EPSILON && x <= east + MERCATOR_EPSILON);
                assert!(y >= south - MERCATOR_EPSILON && y <= north + MERCATOR_EPSILON);
            }
        }
    }

    #[test]
    fn every_line_piece_belongs_to_exactly_one_cell() {
        let scene = scene().expect("the checked-in asset parses");
        for (layer, name) in [(&scene.borders, "borders"), (&scene.roads, "roads")] {
            let mut covered = vec![0u32; layer.pieces.len()];
            for range in &layer.cells {
                for index in range.clone() {
                    covered[index as usize] += 1;
                }
            }
            for (index, count) in covered.iter().enumerate() {
                assert_eq!(*count, 1, "{name} piece {index} is in {count} cells");
            }
        }
    }

    #[test]
    fn every_line_piece_stays_inside_its_cells_box() {
        let scene = scene().expect("the checked-in asset parses");
        let raw = raw_sections();
        for (layer, section, name) in [
            (&scene.borders, &raw[4], "borders"),
            (&scene.roads, &raw[5], "roads"),
        ] {
            for (index, range) in layer.cells.iter().enumerate() {
                let [west, south, east, north] = cell_box(index as u32);
                for piece in &layer.pieces[range.start as usize..range.end as usize] {
                    for offset in 0..usize::from(piece.len) {
                        let at = (piece.start as usize + offset) * POINT_LEN;
                        let [longitude, latitude] = read_point(section.points, at);
                        let longitude = f64::from(longitude);
                        let latitude = f64::from(latitude);
                        assert!(
                            longitude >= west - DEGREES_EPSILON
                                && longitude <= east + DEGREES_EPSILON,
                            "{name} piece in cell {index} reaches {longitude}°, \
                             outside {west}..{east}°",
                        );
                        assert!(
                            latitude >= south - DEGREES_EPSILON
                                && latitude <= north + DEGREES_EPSILON,
                            "{name} piece in cell {index} reaches {latitude}°, \
                             outside {south}..{north}°",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_road_pieces_carry_the_expected_flags_and_lengths() {
        let scene = scene().expect("the checked-in asset parses");
        let roads = &scene.roads;

        for piece in &scene.borders.pieces {
            assert_eq!(piece.flags, 0, "a border carries no road flags");
            assert_eq!(piece.length_m, 0, "a border carries no length");
        }

        // Every flag bit the format defines is set and cleared somewhere in the asset:
        // bit 0 by the primary roads, bit 1 by the middle copy, bit 2 by the coarse copy
        // and bit 3 by the fine copy's short chains.
        let mut bits = [[false; 2]; 4];
        let mut longest = 0;
        for piece in &roads.pieces {
            for (slot, bit) in [0usize, 1, 2, 3].into_iter().enumerate() {
                bits[slot][usize::from(piece.flags >> bit & 1)] = true;
            }
            longest = longest.max(piece.length_m);
        }
        assert_eq!(
            bits, [[true; 2]; 4],
            "every defined road bit is set and cleared",
        );
        assert!(
            longest >= 30_000,
            "length_m is metres, not kilometres: the longest piece is {longest}",
        );

        // The four runtime LODs the design filters to, counted straight from the
        // packed flags. A piece of `n` points is `n - 1` segments and each segment is
        // two triangles, so a LOD is `2 × (points - pieces)`.
        fn lod(pieces: &[Piece], keep: fn(&Piece) -> bool) -> (usize, u32, usize) {
            let mut count = 0;
            let mut points = 0;
            for piece in pieces.iter().filter(|piece| keep(piece)) {
                count += 1;
                points += u32::from(piece.len);
            }
            (count, points, 2 * (points as usize - count))
        }

        assert_eq!(
            lod(&roads.pieces, |piece| piece.flags & 0b100 != 0),
            (4_288, 10_596, 12_616),
            "COARSE: the chains at least the build's 0.3 km threshold long",
        );
        assert_eq!(
            lod(&roads.pieces, |piece| piece.flags & 0b10 != 0),
            (4_389, 12_593, 16_408),
            "MIDDLE: the same chains at 0.004°",
        );
        assert_eq!(
            lod(&roads.pieces, |piece| piece.flags & 0b111 == 0),
            (7_728, 22_283, 29_110),
            "FINE: every class-0 chain, including the short ones the runtime gates",
        );
        assert_eq!(
            lod(&roads.pieces, |piece| piece.flags & 0b110 == 0),
            (13_071, 37_051, 47_960),
            "DE: the fine copy plus the primary roads",
        );
    }

    #[test]
    fn no_ring_point_is_merged_away_by_the_tessellator() {
        let scene = scene().expect("the checked-in asset parses");
        let raw = raw_sections();

        let mut ring: Vec<[u32; 2]> = raw[0]
            .points
            .as_chunks::<POINT_LEN>()
            .0
            .iter()
            .map(|chunk| {
                let [longitude, latitude] = read_point(chunk, 0);
                let [x, y] = model::project(longitude, latitude);
                [x.to_bits(), y.to_bits()]
            })
            .collect();
        ring.sort_unstable();
        ring.dedup();

        let mut vertices: Vec<[u32; 2]> = scene.land[0]
            .triangles
            .iter()
            .chain(scene.land[0].wide.iter())
            .flat_map(|triangle| triangle.as_chunks::<2>().0)
            .map(|pair| [pair[0].to_bits(), pair[1].to_bits()])
            .collect();
        vertices.sort_unstable();
        vertices.dedup();

        assert_eq!(
            ring.len(),
            LAND_RING_POINTS,
            "one closing point per ring is stored twice",
        );
        assert_eq!(
            vertices.len(),
            LAND_TRIANGLE_POINTS,
            "the tessellator skipped a different number of ring points",
        );
        assert!(
            vertices
                .iter()
                .all(|point| ring.binary_search(point).is_ok()),
            "the tessellator invented a vertex that is not a ring point",
        );

        // The only ring points the triangles do not carry are the ones lyon treats as
        // collinear with their neighbours: a vertex within `TESSELLATION_TOLERANCE` of
        // the line through the two points beside it is skipped, which moves no edge by
        // more than the tolerance. At `1e-6` two points are skipped; at `1e-7` none
        // are. One pixel at four times the fitted zoom is about 4e-5 Mercator radians,
        // so both are far below what the map can draw, and the coarser tolerance is
        // kept for the cheaper tessellation.
        let mut skipped = 0;
        for piece in raw[0].pieces.as_chunks::<PIECE_LEN>().0 {
            let start = read_u32(piece, 0) as usize;
            let len = usize::from(read_u16(piece, 4));
            let projected: Vec<[f32; 2]> = (start..start + len)
                .map(|index| {
                    let [longitude, latitude] = read_point(raw[0].points, index * POINT_LEN);
                    model::project(longitude, latitude)
                })
                .collect();
            for (index, point) in projected.iter().enumerate() {
                let key = [point[0].to_bits(), point[1].to_bits()];
                if vertices.binary_search(&key).is_ok() {
                    continue;
                }
                skipped += 1;
                let previous = projected[(index + len - 1) % len];
                let next = projected[(index + 1) % len];
                let distance = distance_to_line(*point, previous, next);
                // Twice the tolerance, not once: the worst of the two is 1.014e-6 rad,
                // and the cross product this is computed from is a difference of two
                // products of `f32` coordinates, good to a few percent here.
                assert!(
                    distance <= 2.0 * TESSELLATION_TOLERANCE,
                    "a ring point was merged away instead of skipped as collinear: \
                     {distance} rad from the line through its neighbours",
                );
            }
        }
        assert_eq!(skipped, LAND_SKIPPED_POINTS);
    }

    /// The distance from `point` to the line through `from` and `to`, in Mercator radians.
    fn distance_to_line(point: [f32; 2], from: [f32; 2], to: [f32; 2]) -> f32 {
        let span = [to[0] - from[0], to[1] - from[1]];
        let length = (span[0] * span[0] + span[1] * span[1]).sqrt();
        if length == 0.0 {
            return 0.0;
        }
        let cross = (point[0] - from[0]) * span[1] - (point[1] - from[1]) * span[0];
        (cross / length).abs()
    }

    /// The Mercator rectangle a 1,600 × 866 viewport shows at four times the fitted
    /// zoom, centred on the strait: the view the design measures the culling with.
    fn strait_viewport() -> [f32; 4] {
        let scale = model::fit_px_per_merc([1_600.0, 866.0]) * model::MAX_ZOOM;
        let centre = model::project(56.30, 26.55);
        let half = [800.0 / scale, 433.0 / scale];
        [
            centre[0] - half[0],
            centre[1] - half[1],
            centre[0] + half[0],
            centre[1] + half[1],
        ]
    }

    #[test]
    fn a_bucketed_triangle_stays_within_one_cell_of_every_cell_its_box_touches() {
        // This is what makes the land's one-cell margin enough. The cells are read
        // through `visible_cells`, the convention the frame uses, not through the
        // bucket's own helpers: the tightest viewport that can touch the triangle is a
        // point at one of the corners of its box, and the centroid cell has to be inside
        // that point's cell grown by one.
        let scene = scene().expect("the checked-in asset parses");
        let [west, _, east, _] = model::mercator_bounds();
        for land in &scene.land {
            for triangle in &land.triangles {
                let [min_x, min_y, max_x, max_y] = triangle_bounds(triangle);
                let centroid = row_of((triangle[1] + triangle[3] + triangle[5]) / 3.0) * GRID_COLS
                    + column_of((triangle[0] + triangle[2] + triangle[4]) / 3.0, west, east);
                for (x, y) in [
                    (min_x, min_y),
                    (max_x, max_y),
                    (min_x, max_y),
                    (max_x, min_y),
                ] {
                    assert!(
                        model::visible_cells([x, y, x, y])
                            .grown()
                            .indices()
                            .any(|cell| cell == centroid),
                        "a bucketed triangle's centroid cell is more than one cell from a \
                         corner of its box",
                    );
                }
            }
        }
        assert!(
            !scene.land[0].triangles.is_empty() && !scene.land[0].wide.is_empty(),
            "both halves of the finest layer are exercised",
        );
    }

    #[test]
    fn a_triangle_the_viewport_touches_is_drawn_even_when_its_centroid_cell_is_not() {
        // The tessellator emits triangles spanning most of the map. Bucketed by centroid
        // alone they would be missing from any viewport that touches them but not their
        // centroid: at four times the fitted zoom over the strait, 561 of the 4,664
        // triangles the finest viewport touches (12%) have their centroid in a cell the
        // ungrown span does not visit, and they carry 69% of the land area it shows.
        // Every tier has wide triangles of its own (3,389 / 1,591 / 933), so the wide
        // path is exercised at all three.
        let scene = scene().expect("the checked-in asset parses");
        let visible = strait_viewport();
        let cells = model::visible_cells(visible).grown();
        let [west, _, east, _] = model::mercator_bounds();

        for (tier, land) in scene.land.iter().enumerate() {
            assert!(!land.wide.is_empty(), "tier {tier} has no wide triangles");

            let mut drawn: Vec<*const [f32; 6]> = land
                .visible(cells, visible)
                .map(|triangle| triangle as *const [f32; 6])
                .collect();
            drawn.sort_unstable();

            let mut overlapping = 0;
            for triangle in land.triangles.iter().chain(land.wide.iter()) {
                let [west, south, east, north] = triangle_bounds(triangle);
                if west <= visible[2]
                    && east >= visible[0]
                    && south <= visible[3]
                    && north >= visible[1]
                {
                    overlapping += 1;
                    assert!(
                        drawn.binary_search(&(triangle as *const [f32; 6])).is_ok(),
                        "tier {tier}: the viewport touches a triangle the frame would not draw",
                    );
                }
            }
            assert!(overlapping > 0, "tier {tier}: the viewport shows some land");

            // At least one of the tier's wide triangles is drawn while its centroid cell
            // is not one the viewport visits: that is the case `visible` keeps `wide`
            // for, and it has to hold at every tier, not only at the finest one.
            let escaped = land.wide.iter().find(|triangle| {
                let centroid = [
                    (triangle[0] + triangle[2] + triangle[4]) / 3.0,
                    (triangle[1] + triangle[3] + triangle[5]) / 3.0,
                ];
                let centroid_cell =
                    row_of(centroid[1]) * GRID_COLS + column_of(centroid[0], west, east);
                !cells.indices().any(|cell| cell == centroid_cell)
                    && drawn.binary_search(&(*triangle as *const [f32; 6])).is_ok()
            });
            assert!(
                escaped.is_some(),
                "tier {tier}: no wide triangle is drawn from outside the visited cells",
            );
        }

        // At the fitted zoom the whole map is visible, so every triangle of every tier is
        // drawn once.
        let whole = model::mercator_bounds();
        let all = model::visible_cells(whole).grown();
        for (tier, land) in scene.land.iter().enumerate() {
            assert_eq!(
                land.visible(all, whole).count(),
                LAND_TRIANGLES[tier],
                "a frame that sees the whole map draws every triangle of tier {tier} once",
            );
        }
    }

    #[test]
    fn every_truncated_prefix_of_the_asset_is_rejected() {
        // A truncated asset has to fail before the land reaches the tessellator rather
        // than panic in it. Sampled rather than every byte: the tail is dense because a
        // cut there still leaves every section's header readable.
        let mut cuts: Vec<usize> = (0..MAP_ASSET.len()).step_by(512).collect();
        cuts.extend(MAP_ASSET.len().saturating_sub(1_024)..MAP_ASSET.len());
        for cut in cuts {
            assert_eq!(
                Scene::parse(&MAP_ASSET[..cut]).err(),
                Some(SceneError::Truncated),
                "a prefix of {cut} bytes should be truncated",
            );
        }
    }

    #[test]
    fn a_flipped_header_never_panics() {
        // A flipped byte is either rejected or read as a different but valid value; what
        // it must never do is panic or over-read. The header and every section header are
        // the bytes every count and offset is read from.
        let raw = raw_sections();
        let base = MAP_ASSET.as_ptr() as usize;
        let mut positions: Vec<usize> = (0..HEADER_LEN).collect();
        for section in &raw {
            let points = section.points.as_ptr() as usize - base;
            positions.extend(points - SECTION_HEADER_LEN..points);
        }
        assert_eq!(
            positions.len(),
            HEADER_LEN + SECTION_COUNT as usize * SECTION_HEADER_LEN,
        );

        let mut bytes = MAP_ASSET.to_vec();
        for position in positions {
            for flip in [0x01, 0xff] {
                bytes[position] ^= flip;
                let _ = Scene::parse(&bytes);
                bytes[position] ^= flip;
            }
        }
    }

    #[test]
    fn a_flipped_asset_never_panics() {
        // Every kind of section, byte-flipped: the parser may read the change as another
        // valid value (a coordinate is just a number) but it must never panic or
        // over-read. Sampled every sixteenth byte to keep it cheap; the two cell tables
        // are 16 KiB of this fixture each, and they are where the range arithmetic lives.
        let asset = line_asset();
        let mut bytes = asset.clone();
        for position in (0..asset.len()).step_by(16) {
            for flip in [0x01, 0xff] {
                bytes[position] ^= flip;
                let _ = Scene::parse(&bytes);
                bytes[position] ^= flip;
            }
        }
    }

    #[test]
    fn mercator_is_monotonic_over_the_map() {
        let south = model::project(model::MAP_WEST, model::MAP_SOUTH);
        let north = model::project(model::MAP_WEST, model::MAP_NORTH);
        assert!(north[1] > south[1]);

        let west = model::project(model::MAP_WEST, model::MAP_SOUTH);
        let east = model::project(model::MAP_EAST, model::MAP_SOUTH);
        assert!(east[0] > west[0]);

        let mut previous = model::MAP_WEST;
        for step in 1..=32 {
            let longitude =
                model::MAP_WEST + (model::MAP_EAST - model::MAP_WEST) * step as f32 / 32.0;
            let [x, _] = model::project(longitude, model::MAP_SOUTH);
            let [previous_x, _] = model::project(previous, model::MAP_SOUTH);
            assert!(x > previous_x);
            previous = longitude;
        }

        let mut previous = model::MAP_SOUTH;
        for step in 1..=32 {
            let latitude =
                model::MAP_SOUTH + (model::MAP_NORTH - model::MAP_SOUTH) * step as f32 / 32.0;
            let [_, y] = model::project(model::MAP_WEST, latitude);
            let [_, previous_y] = model::project(model::MAP_WEST, previous);
            assert!(y > previous_y);
            previous = latitude;
        }
    }

    #[test]
    fn repeated_loads_share_one_scene() {
        let first = scene().expect("the checked-in asset parses");
        let second = scene().expect("the checked-in asset parses");
        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn a_bad_magic_is_rejected() {
        let mut bytes = triangle_asset();
        bytes[3] = b'0';
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::InvalidMagic));
    }

    #[test]
    fn an_unknown_version_is_rejected() {
        // Version 3 is the shape this parser reads. The earlier versions held one and
        // three land sections; they are not migrated — the asset is rebuilt in the same
        // change that bumps the version — so the previous ones and the next one both fail
        // here, as does any other number.
        for version in [1u32, 2, 4, u32::MAX] {
            let mut bytes = triangle_asset();
            bytes[4..8].copy_from_slice(&version.to_le_bytes());
            assert_eq!(
                Scene::parse(&bytes).err(),
                Some(SceneError::UnsupportedVersion),
                "version {version} should be rejected",
            );
        }
    }

    #[test]
    fn a_bbox_that_is_not_the_map_box_is_rejected() {
        let mut bytes = triangle_asset();
        bytes[8..12].copy_from_slice(&47.9f32.to_le_bytes());
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::BboxMismatch));

        let mut bytes = triangle_asset();
        bytes[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::BboxMismatch));
    }

    #[test]
    fn a_grid_that_is_not_the_map_grid_is_rejected() {
        let mut bytes = triangle_asset();
        bytes[24..28].copy_from_slice(&32u32.to_le_bytes());
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::GridMismatch));
    }

    /// A previous asset version is a different file shape — one land section in version
    /// 1, three in version 2, four in version 3 — and this parser does not migrate it:
    /// the checked-in asset is rebuilt by `assets/build_map.py` in the same change that
    /// moves the version. Rewriting the version byte of the real asset therefore has to
    /// be rejected before any section is read, not read as a six-section file that
    /// happens to start with four land sections.
    #[test]
    fn the_previous_asset_version_is_rejected_at_the_header() {
        for version in [1u32, 2, 0] {
            let mut bytes = MAP_ASSET.to_vec();
            bytes[4..8].copy_from_slice(&version.to_le_bytes());
            assert_eq!(
                Scene::parse(&bytes).err(),
                Some(SceneError::UnsupportedVersion),
                "the asset rewritten as version {version} should be rejected",
            );
        }
        assert!(Scene::parse(MAP_ASSET).is_ok(), "the asset itself parses");
    }

    #[test]
    fn a_missing_section_is_rejected() {
        // The header declares fewer sections than the format has.
        let mut bytes = triangle_asset();
        bytes[32..36].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::MissingSection));

        // Every section is expected where the format puts it: a file that swapped any
        // two of them — a land tier for the borders, two tiers the other way round — is
        // not read as a different map.
        let raw = raw_sections();
        let base = MAP_ASSET.as_ptr() as usize;
        for (index, section) in raw.iter().enumerate() {
            let section_offset = section.points.as_ptr() as usize - base - SECTION_HEADER_LEN;
            let wrong = ALL[(index + 1) % SECTION_COUNT as usize] as u32;
            let mut swapped = MAP_ASSET.to_vec();
            swapped[section_offset..section_offset + 4].copy_from_slice(&wrong.to_le_bytes());
            assert_eq!(
                Scene::parse(&swapped).err(),
                Some(SceneError::MissingSection),
                "section {index} reading as kind {wrong} should be rejected",
            );
        }

        // The header declares a seventh section the format does not define.
        let mut bytes = triangle_asset();
        bytes[32..36].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::MissingSection));
    }

    /// The byte range of each section of the checked-in asset.
    fn section_ranges() -> Vec<Range<usize>> {
        let raw = raw_sections();
        let base = MAP_ASSET.as_ptr() as usize;
        let starts: Vec<usize> = raw
            .iter()
            .map(|section| section.points.as_ptr() as usize - base - SECTION_HEADER_LEN)
            .collect();
        starts
            .iter()
            .copied()
            .zip(starts.iter().copied().skip(1).chain([MAP_ASSET.len()]))
            .map(|(start, end)| start..end)
            .collect()
    }

    #[test]
    fn a_physically_missing_section_is_rejected() {
        // The header still declares six sections; the bytes hold five. Removing one from
        // the middle shifts every later section under the wrong expected kind, which the
        // kind check catches; removing the last runs out of bytes, which is truncation.
        let ranges = section_ranges();
        assert_eq!(ranges.len(), SECTION_COUNT as usize);

        let mut missing_middle = MAP_ASSET.to_vec();
        missing_middle.drain(ranges[2].clone());
        assert_eq!(
            Scene::parse(&missing_middle).err(),
            Some(SceneError::MissingSection),
            "a file without its third section is read as a different map",
        );

        let mut missing_last = MAP_ASSET.to_vec();
        missing_last.truncate(ranges[5].start);
        assert_eq!(
            Scene::parse(&missing_last).err(),
            Some(SceneError::Truncated),
            "a file that ends before its sixth section was never packed whole",
        );
    }

    #[test]
    fn a_land_section_with_a_cell_table_is_rejected() {
        // The format reserves the cell table for the line layers: the land's grid is made
        // from its triangles at parse time. A land section that declares one is malformed
        // even if the table itself happens to be valid, and must not be silently ignored.
        let mut land = land_triangle();
        land[1].cells = vec![(0, 1)];
        let bytes = asset(
            land,
            empty_line_section(SectionKind::Borders),
            empty_line_section(SectionKind::Roads),
        );
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::LandHasCells));
    }

    #[test]
    fn every_truncated_prefix_is_rejected() {
        let bytes = triangle_asset();
        assert!(
            Scene::parse(&bytes).is_ok(),
            "the fixture itself is a valid asset",
        );
        for cut in 0..bytes.len() {
            assert_eq!(
                Scene::parse(&bytes[..cut]).err(),
                Some(SceneError::Truncated),
                "a prefix of {cut} bytes should be truncated",
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = triangle_asset();
        bytes.push(0);
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::TrailingBytes));
    }

    #[test]
    fn an_asset_without_land_is_empty() {
        let bytes = asset(
            land_tiers(&[], &[]),
            empty_line_section(SectionKind::Borders),
            empty_line_section(SectionKind::Roads),
        );
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::Empty));
    }

    #[test]
    fn an_asset_that_tessellates_to_nothing_is_empty() {
        let bytes = ring_asset(&[[50.0, 25.0], [50.0, 25.0], [50.0, 25.0]]);
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::Empty));
    }

    #[test]
    fn a_non_finite_coordinate_is_rejected() {
        let points = [[50.0f32, 25.0], [51.0, 25.0], [50.0, 26.0], [50.0, 25.0]];
        for [longitude, latitude] in [
            [f32::NAN, 25.0],
            [50.0, f32::NAN],
            [f32::INFINITY, 25.0],
            [50.0, f32::INFINITY],
        ] {
            let mut malformed = points;
            malformed[0] = [longitude, latitude];
            assert_eq!(
                Scene::parse(&ring_asset(&malformed)).err(),
                Some(SceneError::NonFiniteCoordinate),
            );
        }

        let borders = line_section(
            SectionKind::Borders,
            &[[50.0, 25.0], [f32::NAN, 25.0]],
            &[(0, 2, 0, 0)],
            one_cell(1),
        );
        let bytes = asset(
            land_triangle(),
            borders,
            empty_line_section(SectionKind::Roads),
        );
        assert_eq!(
            Scene::parse(&bytes).err(),
            Some(SceneError::NonFiniteCoordinate),
        );
    }

    #[test]
    fn a_piece_that_reaches_past_its_points_is_rejected() {
        // A land ring that claims a point the section does not hold.
        let bytes = asset(
            land_tiers(&[[50.0, 25.0], [51.0, 25.0], [50.0, 26.0]], &[(0, 4)]),
            empty_line_section(SectionKind::Borders),
            empty_line_section(SectionKind::Roads),
        );
        assert_eq!(
            Scene::parse(&bytes).err(),
            Some(SceneError::PointCountOverflow),
        );

        // A border piece that starts past the end of its points.
        let borders = line_section(
            SectionKind::Borders,
            &[[50.0, 25.0], [51.0, 25.0]],
            &[(1, 2, 0, 0)],
            one_cell(1),
        );
        let bytes = asset(
            land_triangle(),
            borders,
            empty_line_section(SectionKind::Roads),
        );
        assert_eq!(
            Scene::parse(&bytes).err(),
            Some(SceneError::PointCountOverflow),
        );
    }

    #[test]
    fn a_bad_cell_table_is_rejected() {
        // The table does not have one range per cell.
        let roads = line_section(
            SectionKind::Roads,
            &[[50.0, 25.0], [51.0, 25.0]],
            &[(0, 2, 0, 1_000)],
            vec![(0, 1)],
        );
        let bytes = asset(
            land_triangle(),
            empty_line_section(SectionKind::Borders),
            roads,
        );
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::BadCellRange));

        // The ranges leave a gap: piece 1 is in no cell.
        let mut cells = vec![(0, 0); CELLS as usize];
        cells[0] = (0, 1);
        cells[1] = (2, 0);
        let roads = line_section(
            SectionKind::Roads,
            &[[50.0, 25.0], [51.0, 25.0], [52.0, 25.0]],
            &[(0, 2, 0, 1_000), (2, 1, 0, 500)],
            cells,
        );
        let bytes = asset(
            land_triangle(),
            empty_line_section(SectionKind::Borders),
            roads,
        );
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::BadCellRange));

        // The ranges overlap: piece 1 would be in two cells.
        let mut cells = vec![(0, 0); CELLS as usize];
        cells[0] = (0, 2);
        cells[1] = (1, 1);
        let roads = line_section(
            SectionKind::Roads,
            &[[50.0, 25.0], [51.0, 25.0], [52.0, 25.0]],
            &[(0, 2, 0, 1_000), (2, 1, 0, 500)],
            cells,
        );
        let bytes = asset(
            land_triangle(),
            empty_line_section(SectionKind::Borders),
            roads,
        );
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::BadCellRange));
    }

    #[test]
    fn a_piece_in_no_cell_is_rejected() {
        // Two pieces, but the table only reaches the first.
        let roads = line_section(
            SectionKind::Roads,
            &[[50.0, 25.0], [51.0, 25.0], [52.0, 25.0]],
            &[(0, 2, 0, 1_000), (2, 1, 0, 500)],
            one_cell(1),
        );
        let bytes = asset(
            land_triangle(),
            empty_line_section(SectionKind::Borders),
            roads,
        );
        assert_eq!(
            Scene::parse(&bytes).err(),
            Some(SceneError::PieceNotInAnyCell),
        );
    }

    #[test]
    fn a_piece_with_undefined_metadata_is_rejected() {
        // A border carries no road flags.
        let borders = line_section(
            SectionKind::Borders,
            &[[50.0, 25.0], [51.0, 25.0]],
            &[(0, 2, 1, 0)],
            one_cell(1),
        );
        let bytes = asset(
            land_triangle(),
            borders,
            empty_line_section(SectionKind::Roads),
        );
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::BadPiece));

        // A road carries only the four flag bits the format defines (0, 1, 2, 3); bits
        // 4 and above are undefined.
        for flags in [16, 32] {
            let roads = line_section(
                SectionKind::Roads,
                &[[50.0, 25.0], [51.0, 25.0]],
                &[(0, 2, flags, 1_000)],
                one_cell(1),
            );
            let bytes = asset(
                land_triangle(),
                empty_line_section(SectionKind::Borders),
                roads,
            );
            assert_eq!(
                Scene::parse(&bytes).err(),
                Some(SceneError::BadPiece),
                "flags {flags:#04b}",
            );
        }

        // A land piece carries no metadata at all.
        let mut land = land_triangle();
        land[0].reserved = 1;
        let bytes = asset(
            land,
            empty_line_section(SectionKind::Borders),
            empty_line_section(SectionKind::Roads),
        );
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::BadPiece));
    }

    /// The Mercator rectangle a camera centred on `centre` shows at `zoom`.
    fn viewport_rect(centre: [f32; 2], viewport: [f32; 2], zoom: f32) -> [f32; 4] {
        let scale = model::fit_px_per_merc(viewport) * zoom;
        let half = [viewport[0] / (2.0 * scale), viewport[1] / (2.0 * scale)];
        [
            centre[0] - half[0],
            centre[1] - half[1],
            centre[0] + half[0],
            centre[1] + half[1],
        ]
    }

    /// The Mercator `x` of a column grid line.
    fn column_edge(column: u32) -> f32 {
        let [west, _, east, _] = model::mercator_bounds();
        west + (east - west) * column as f32 / GRID_COLS as f32
    }

    /// The Mercator `y` of a row grid line.
    ///
    /// The rows are latitude bands, so the line is cut in degrees and projected; reading
    /// it off the Mercator `y` would move it by up to a quarter of a row.
    fn row_edge(row: u32) -> f32 {
        let latitude = model::MAP_SOUTH
            + (model::MAP_NORTH - model::MAP_SOUTH) * row as f32 / GRID_ROWS as f32;
        model::project(model::MAP_WEST, latitude)[1]
    }

    /// Whether a triangle's box touches `visible`.
    fn touches(triangle: &[f32; 6], visible: [f32; 4]) -> bool {
        let [west, south, east, north] = triangle_bounds(triangle);
        west <= visible[2] && east >= visible[0] && south <= visible[3] && north >= visible[1]
    }

    /// The index of one of the layer's triangles, `triangles` first and then `wide`.
    fn index_of(land: &LandLayer, triangle: &[f32; 6]) -> Option<usize> {
        let offset = |slice: &[[f32; 6]]| {
            let base = slice.as_ptr() as usize;
            let address = triangle.as_ptr() as usize;
            let size = std::mem::size_of::<[f32; 6]>();
            let inside = address >= base && address < base + std::mem::size_of_val(slice);
            (inside && (address - base).is_multiple_of(size)).then(|| (address - base) / size)
        };
        offset(&land.triangles)
            .or_else(|| offset(&land.wide).map(|index| index + land.triangles.len()))
    }

    /// The viewports the completeness sweep visits.
    ///
    /// The named ones are the design's: the fitted whole map, two and four times the
    /// fitted zoom at the map centre, Dubai and the strait, the four corners, and
    /// rectangles whose edges or centres lie on grid lines. The rest are a deterministic
    /// sweep over centres, sizes and zooms, a tenth of the map past every edge, so some
    /// of them hang off the map entirely.
    fn sweep_viewports() -> Vec<(String, [f32; 4])> {
        let viewport = [1_600.0, 866.0];
        let [west, south, east, north] = model::mercator_bounds();
        let centre = [(west + east) / 2.0, (south + north) / 2.0];
        let at = |longitude: f32, latitude: f32| model::project(longitude, latitude);

        let mut rects = vec![
            (
                "1x whole map".to_owned(),
                viewport_rect(centre, viewport, 1.0),
            ),
            ("2x centre".to_owned(), viewport_rect(centre, viewport, 2.0)),
            ("4x centre".to_owned(), viewport_rect(centre, viewport, 4.0)),
            (
                "4x dubai".to_owned(),
                viewport_rect(at(55.30, 25.20), viewport, 4.0),
            ),
            (
                "4x strait".to_owned(),
                viewport_rect(at(56.30, 26.55), viewport, 4.0),
            ),
            (
                "4x south-west corner".to_owned(),
                viewport_rect(at(model::MAP_WEST, model::MAP_SOUTH), viewport, 4.0),
            ),
            (
                "4x south-east corner".to_owned(),
                viewport_rect(at(model::MAP_EAST, model::MAP_SOUTH), viewport, 4.0),
            ),
            (
                "4x north-west corner".to_owned(),
                viewport_rect(at(model::MAP_WEST, model::MAP_NORTH), viewport, 4.0),
            ),
            (
                "4x north-east corner".to_owned(),
                viewport_rect(at(model::MAP_EAST, model::MAP_NORTH), viewport, 4.0),
            ),
            // Four columns by five rows, exactly on the grid lines.
            (
                "grid-aligned".to_owned(),
                [column_edge(16), row_edge(8), column_edge(20), row_edge(13)],
            ),
            // One cell, and a sliver a hundred-and-twenty-eighth of a column wide.
            (
                "one cell".to_owned(),
                [column_edge(32), row_edge(16), column_edge(33), row_edge(17)],
            ),
            (
                "half-cell sliver".to_owned(),
                [
                    column_edge(32),
                    row_edge(16),
                    column_edge(32) + (east - west) / 128.0,
                    row_edge(16) + (north - south) / 64.0,
                ],
            ),
        ];

        // Viewports whose centre is a grid-line crossing, so their edges straddle lines.
        for (column, row) in [(0, 0), (32, 16), (63, 31), (17, 9), (48, 24)] {
            rects.push((
                format!("4x centred on grid line {column}/{row}"),
                viewport_rect([column_edge(column), row_edge(row)], viewport, 4.0),
            ));
        }

        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 11) as f32 / (1u64 << 53) as f32
        };
        for index in 0..120 {
            let centre = [
                west - 0.1 + (east - west + 0.2) * next(),
                south - 0.1 + (north - south + 0.2) * next(),
            ];
            let size = [200.0 + 1_600.0 * next(), 120.0 + 900.0 * next()];
            rects.push((
                format!("sweep {index}"),
                viewport_rect(centre, size, 1.0 + 3.0 * next()),
            ));
        }
        rects
    }

    #[test]
    fn every_viewport_draws_every_triangle_it_touches_and_draws_none_twice() {
        // The frame draws the land from the cells the viewport touches plus one cell of
        // margin, and tests the wide triangles against the viewport itself. This is the
        // ground truth for that, tier by tier: a triangle whose box the viewport touches
        // must be drawn, and no triangle may be drawn twice — the two halves are
        // disjoint, so a wide triangle cannot also come back through its cell.
        let scene = scene().expect("the checked-in asset parses");
        let mut viewports = 0;
        for (tier, land) in scene.land.iter().enumerate() {
            let all: Vec<(&[f32; 6], [f32; 4])> = land
                .triangles
                .iter()
                .chain(land.wide.iter())
                .map(|triangle| (triangle, triangle_bounds(triangle)))
                .collect();
            assert_eq!(all.len(), LAND_TRIANGLES[tier]);

            let mut draws = vec![0u32; LAND_TRIANGLES[tier]];
            for (label, visible) in sweep_viewports() {
                viewports += 1;
                let cells = model::visible_cells(visible);
                for triangle in land.visible(cells.grown(), visible) {
                    let index = index_of(land, triangle).expect("a triangle of the layer");
                    draws[index] += 1;
                }

                for (index, (triangle, _)) in all.iter().enumerate() {
                    if touches(triangle, visible) {
                        assert!(
                            draws[index] > 0,
                            "{label} touches a triangle it does not draw (tier {tier})",
                        );
                    }
                    assert!(
                        draws[index] <= 1,
                        "{label} draws a triangle of tier {tier} {} times",
                        draws[index],
                    );
                    draws[index] = 0;
                }
            }
        }
        assert!(viewports >= 100, "the sweep lost viewports");
    }

    #[test]
    fn every_line_piece_sits_in_the_cell_the_build_packed_it_into() {
        // The asset's own cell table is the oracle for the row convention. The build cut
        // the rows in degrees, with row 0 at the south edge, so the runtime has to read
        // the row through `latitude_of` rather than off the Mercator `y`: the two axes
        // differ by up to a quarter of a row, and reading the `y` would put most of these
        // pieces in the cell beside the one the build wrote.
        //
        // The centroid is recomputed here the way the build saw it (the mean of the f64
        // degrees, then the projection the runtime reads back), and that round trip is
        // only the identity to within `f32` noise. A piece the build clipped along a row
        // line has a centroid a hair on one side of it — one road piece in cell 734 sits
        // 5.3e-6 rows north of the row 10/11 line — and the noise can put it on the other
        // side. Such a piece must still be read as one of the two rows the line joins,
        // never as a cell further away.
        const ROUND_TRIP_EPSILON_ROWS: f64 = 5e-5;

        let scene = scene().expect("the checked-in asset parses");
        let raw = raw_sections();
        let [west, _, east, _] = model::mercator_bounds();
        let mut pieces = 0;
        for (layer, section, name) in [
            (&scene.borders, &raw[4], "borders"),
            (&scene.roads, &raw[5], "roads"),
        ] {
            for (cell, range) in layer.cells.iter().enumerate() {
                for piece in &layer.pieces[range.start as usize..range.end as usize] {
                    pieces += 1;
                    let mut longitude = 0.0f64;
                    let mut latitude = 0.0f64;
                    for offset in 0..usize::from(piece.len) {
                        let at = (piece.start as usize + offset) * POINT_LEN;
                        let [lon, lat] = read_point(section.points, at);
                        longitude += f64::from(lon);
                        latitude += f64::from(lat);
                    }
                    let count = f64::from(piece.len);
                    let [x, y] =
                        model::project((longitude / count) as f32, (latitude / count) as f32);
                    let row = row_of(y);
                    let packed_row = cell as u32 / GRID_COLS;
                    let row_is_the_packed_one_or_its_neighbour = if row == packed_row {
                        true
                    } else {
                        let fraction = (latitude / count - f64::from(model::MAP_SOUTH))
                            / f64::from(model::MAP_NORTH - model::MAP_SOUTH)
                            * f64::from(GRID_ROWS);
                        (fraction - fraction.round()).abs() <= ROUND_TRIP_EPSILON_ROWS
                            && row.abs_diff(packed_row) == 1
                    };
                    assert!(
                        row_is_the_packed_one_or_its_neighbour
                            && column_of(x, west, east) == cell as u32 % GRID_COLS,
                        "{name} piece in cell {cell} is read as another cell",
                    );
                }
            }
        }
        assert_eq!(pieces, (BORDER_PIECES + ROAD_PIECES) as usize);
    }
}
