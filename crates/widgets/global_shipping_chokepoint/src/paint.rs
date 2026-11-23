//! How one frame of the map is drawn: the meshes the picture is built from and the order
//! they stack in.
//!
//! The base map is prepared once by [`crate::scene`] and the picture arrives as a list of
//! vessels, so a frame here only applies the camera's affine transform: the coastline is a
//! triangle list of the one resolution the scale justifies, the borders and the roads are
//! ribbons whose width is in screen pixels, and the borders are dashes walked along their
//! own polylines. Nothing is tessellated per frame and nothing is cached: a camera change
//! is a different set of vertices, not a different picture.
//!
//! # What a frame is
//!
//! A [`Mesh`] is a plain triangle list of positions plus the AABB of those positions. The
//! land is the exception to "per frame": it is the same Web Mercator geometry at every
//! camera, so it is built once per tier ([`land_mesh`]) and a frame only selects the index
//! ranges of the cells it can see ([`visible_land_ranges`]). Everything else is built per
//! frame in window pixels ([`frame_meshes`]), because a camera change really is a different
//! set of vertices.
//!
//! [`crate::map`] is what submits them: it wraps each mesh in a
//! [`GeometryDesc`](ui_core::paint::geometry::GeometryDesc) — the engine's `Ui` pipeline
//! carries `[x, y, coverage distance]` per vertex, and every vertex here is interior at
//! `-1.0` because none of this geometry is anti-aliased — registers it in the engine's
//! geometry store, and pushes a
//! [`GeometryDraw`](ui_core::paint::primitive::GeometryDraw) into its canvas in the order
//! [`in_paint_order`] gives. This module touches no canvas, no `App` and no theme: it turns
//! the model's [`Projection`] into vertex buffers, and [`crate::colors::MapColors`] says
//! which colour each buffer is drawn in.

use std::ops::Range;

use ui_core::{math::Rect, paint::color::Color};

use crate::{
    colors::MapColors,
    model::{self, CellSpan, Projection, RoadFilter, Vessel},
    scene::{LandLayer, LineLayer, Piece, Scene},
    ship::{SHIP_TYPE_COUNT, SHIP_TYPES, ShipType},
};

/// How wide a marker is against its length.
const MARKER_WIDTH_RATIO: f32 = 0.7;

/// The radius of the dot a vessel with no type is drawn as.
const UNKNOWN_VESSEL_RADIUS: f32 = 2.5;

/// How many segments the unknown-vessel dot is drawn with.
///
/// Six rather than eight: the dot is 2.5 px in radius, so a segment spans about
/// 2.6 px of its circumference — still round at the fitted zoom — and a full picture
/// of the whole map's ~2,516 dots costs 15,096 triangles instead of 20,128. That saving
/// is part of what keeps the worst frame under the 75,000-triangle budget.
const ROUND_SEGMENTS: usize = 6;
/// How many segments the hover ring is drawn with.
const RING_SEGMENTS: usize = 24;

/// The inner and outer radius of the ring around the vessel under the pointer.
const HOVER_RING_INNER: f32 = 7.0;
/// The outer radius of the ring around the vessel under the pointer.
const HOVER_RING_OUTER: f32 = 9.0;

/// One dashed stroke: a dash, the gap after it and the stroke's width, all in pixels.
///
/// The pattern is walked along a polyline rather than handed to the engine: an engine
/// geometry is a triangle list, and sampling a dash pattern into it per frame is what the
/// walk here is; the alternative — one tessellation of the whole border at load time —
/// would make a 6 px dash scale with the camera.
///
/// The struct is crate-visible because [`border_mesh`] takes one and that function is
/// `pub(super)`: a private type in its signature would be a `private_interfaces` warning.
/// Its fields stay private, so [`BORDER_DASH`] is the only pattern a caller can pass.
#[derive(Clone, Copy)]
pub(super) struct Dash {
    /// The length of one dash.
    on: f32,
    /// The gap between two dashes.
    off: f32,
    /// The width the dash is drawn at.
    width: f32,
}

/// The dash pattern a land border is drawn with.
///
/// A border is a boundary to follow rather than a road to drive, so it is dashed as well
/// as violet: the pattern is what tells a border from a motorway at a glance when the two
/// run side by side. The dash is coarse enough to read at the fitted zoom, where one pixel
/// is about a kilometre, and the width is the one the old solid stroke had.
const BORDER_DASH: Dash = Dash {
    on: 6.0,
    off: 4.0,
    width: 1.6,
};

/// How wide a motorway or trunk is drawn, in pixels.
const ROAD_WIDTH: f32 = 1.4;

/// How wide a primary road is drawn, in pixels.
const ROAD_MINOR_WIDTH: f32 = 1.2;

/// The marker length at the fitted zoom, and at the closest one, in pixels.
const MARKER_LENGTH_NEAR: f32 = 10.0;
/// The marker length at [`model::MAX_ZOOM`].
const MARKER_LENGTH_FAR: f32 = 12.0;

/// A triangle-list mesh in logical pixels (or Mercator radians for the land), without
/// anti-aliasing: every vertex is interior.
///
/// The engine's `Ui` pipeline is a coverage-distance pipeline, so a mesh that is not
/// anti-aliased is its degenerate case: [`crate::map`] writes `-1.0` — "fully inside" —
/// as the third component of every vertex. The vertices are unshared (index `i` is vertex
/// `i`), which is what lets this module build the triangles straight from the picture and
/// lets a test read the positions back.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Mesh {
    /// One `x, y` position per index, in the space [`Self::bounds`] is in.
    pub vertices: Vec<[f32; 2]>,
    /// The triangle list: three vertex indices per triangle.
    pub indices: Vec<u32>,
    /// The AABB of the vertices, in pixels (Mercator radians for the land).
    pub bounds: Rect,
}

impl Mesh {
    /// Whether the mesh draws nothing.
    ///
    /// The index list is three indices per triangle, so counting triangles is also the empty
    /// test: this is the one place that knows the stride, and a mesh is never built with a
    /// partial triangle.
    pub(super) fn is_empty(&self) -> bool {
        self.triangle_count() == 0
    }

    /// How many triangles the mesh draws.
    pub(super) fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// Accumulates a [`Mesh`]: the positions, the triangle list over them and their AABB.
///
/// The box is the min and max of every vertex rather than a union of rectangles: a union
/// treats a zero-area rectangle as empty, and a degenerate triangle — which rounding can
/// leave behind at the end of a dash — would then drop out of the box the engine damages
/// the frame by.
struct MeshBuilder {
    vertices: Vec<[f32; 2]>,
    indices: Vec<u32>,
    min: [f32; 2],
    max: [f32; 2],
}

impl MeshBuilder {
    /// An empty builder.
    fn new() -> Self {
        Self {
            vertices: Vec::new(),
            indices: Vec::new(),
            min: [f32::INFINITY; 2],
            max: [f32::NEG_INFINITY; 2],
        }
    }

    /// An empty builder that will hold `vertices` and `indices` entries up front.
    ///
    /// Only the builders whose size is known use it: the land's is the tier's own triangle
    /// count and the ring's is fixed, while a frame's dashes and road segments depend on
    /// where the camera is.
    fn with_capacity(vertices: usize, indices: usize) -> Self {
        Self {
            vertices: Vec::with_capacity(vertices),
            indices: Vec::with_capacity(indices),
            ..Self::new()
        }
    }

    /// Appends one triangle: three positions and the indices that draw them.
    fn push_triangle(&mut self, corners: [[f32; 2]; 3]) {
        for [x, y] in corners {
            self.min = [self.min[0].min(x), self.min[1].min(y)];
            self.max = [self.max[0].max(x), self.max[1].max(y)];
            self.indices.push(self.vertices.len() as u32);
            self.vertices.push([x, y]);
        }
    }

    /// Appends one quad as the two triangles that cover it: `0 1 2` and `0 2 3`.
    fn push_quad(&mut self, corners: [[f32; 2]; 4]) {
        self.push_triangle([corners[0], corners[1], corners[2]]);
        self.push_triangle([corners[0], corners[2], corners[3]]);
    }

    /// The mesh built so far.
    ///
    /// A mesh with no vertices has no box ([`Rect::default`]). So does one whose positions
    /// are not finite, which no builder here should produce: the engine clips and damages a
    /// geometry by its box, and an infinite one would be a full-pane repaint every frame
    /// rather than a triangle in the wrong place.
    fn finish(self) -> Mesh {
        let bounds = if self.vertices.is_empty() {
            Rect::default()
        } else {
            Rect::new(
                self.min[0],
                self.min[1],
                self.max[0] - self.min[0],
                self.max[1] - self.min[1],
            )
        };
        Mesh {
            vertices: self.vertices,
            indices: self.indices,
            bounds: if bounds.is_finite() {
                bounds
            } else {
                Rect::default()
            },
        }
    }
}

/// The land of one tier: the whole tier's triangles as one mesh in Mercator radians, in the
/// asset's cell order (cell bucket `k`'s triangles are the ranges in [`Self::cell_indices`]),
/// followed by the wide triangles kept apart by [`LandLayer::wide`].
///
/// One mesh per tier rather than one geometry per cell: the engine draws a static geometry
/// by an index range, so a frame that sees cells 120–126 submits the range those buckets
/// occupy in this one mesh. Splitting the tiers or the buckets into separate geometries would
/// multiply what is registered for no gain — the buffer is already ordered the way the frame
/// walks it.
pub(super) struct LandMesh {
    /// The tier's triangles, in Mercator radians.
    pub mesh: Mesh,
    /// The index range of each cell's triangles, in row-major cell order.
    ///
    /// `GRID_COLS * GRID_ROWS` entries; a cell no triangle's centroid fell in has an empty
    /// range. The ranges are already multiplied by three, so they can be submitted as they
    /// are.
    pub cell_indices: Box<[Range<u32>]>,
    /// The index range of the wide triangles, which are not bucketed by centroid.
    pub wide_indices: Range<u32>,
}

/// Builds one tier's land mesh: every triangle of the tier in cell order, then the wide ones.
///
/// The asset tessellated the coast once and bucketed each triangle by its centroid; this
/// only copies the triangles into a vertex and an index buffer and records where each bucket
/// landed, so the per-frame cost of the coast is the range selection in
/// [`visible_land_ranges`] instead of a vertex per visible point.
pub(super) fn land_mesh(layer: &LandLayer) -> LandMesh {
    let triangle_count = layer.triangles.len() + layer.wide.len();
    let mut builder = MeshBuilder::with_capacity(triangle_count * 3, triangle_count * 3);
    let mut cell_indices = Vec::with_capacity(layer.cells.len());
    for bucket in &layer.cells {
        let start = builder.indices.len() as u32;
        for triangle in &layer.triangles[bucket.start as usize..bucket.end as usize] {
            push_land_triangle(&mut builder, triangle);
        }
        cell_indices.push(start..builder.indices.len() as u32);
    }
    let wide_start = builder.indices.len() as u32;
    for triangle in &layer.wide {
        push_land_triangle(&mut builder, triangle);
    }
    let wide_indices = wide_start..builder.indices.len() as u32;
    LandMesh {
        mesh: builder.finish(),
        cell_indices: cell_indices.into_boxed_slice(),
        wide_indices,
    }
}

/// Appends one of the asset's triangles — three `x, y` pairs in Mercator radians.
fn push_land_triangle(mesh: &mut MeshBuilder, triangle: &[f32; 6]) {
    mesh.push_triangle([
        [triangle[0], triangle[1]],
        [triangle[2], triangle[3]],
        [triangle[4], triangle[5]],
    ]);
}

/// The index ranges one frame draws for the visible part of `land`.
///
/// `cells` is the viewport's grid span grown by one cell, because the land is bucketed by
/// triangle centroid: a triangle whose centroid falls just outside the viewport can still
/// reach into it. The buckets of the cells are contiguous in the mesh — an empty bucket
/// occupies no index — so the cells a frame visits merge into one range rather than one
/// range per cell, and a viewport showing the whole map is the same single range.
///
/// The wide triangles are not bucketed: they are the ones too wide for a centroid bucket, so
/// the frame keeps them by testing their boxes against `visible` — `[west, south, east,
/// north]` in Mercator radians. They are one range because they are one contiguous run at the
/// end of the mesh, and a frame cannot select a subset of them by index: when one of them
/// reaches the viewport the whole run is submitted, so the wide triangles that fall outside
/// the pane are drawn and clipped away — at most the tier's own wide set, 3,389 triangles at
/// the finest tier — in exchange for one range instead of thousands of one-triangle ranges.
/// Nothing visible is no range at all.
pub(super) fn visible_land_ranges(
    land: &LandMesh,
    layer: &LandLayer,
    cells: CellSpan,
    visible: [f32; 4],
) -> Vec<Range<u32>> {
    let mut ranges: Vec<Range<u32>> = Vec::new();
    for cell in cells.indices() {
        let bucket = &land.cell_indices[cell as usize];
        if bucket.start == bucket.end {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if last.end == bucket.start => last.end = bucket.end,
            _ => ranges.push(bucket.start..bucket.end),
        }
    }

    let wide_reaches = layer.wide.iter().any(|triangle| {
        let [west, south, east, north] = triangle_bounds(triangle);
        west <= visible[2] && east >= visible[0] && south <= visible[3] && north >= visible[1]
    });
    if wide_reaches && land.wide_indices.start != land.wide_indices.end {
        ranges.push(land.wide_indices.start..land.wide_indices.end);
    }
    ranges
}

/// The `[west, south, east, north]` box a Mercator triangle covers.
fn triangle_bounds(triangle: &[f32; 6]) -> [f32; 4] {
    [
        triangle[0].min(triangle[2]).min(triangle[4]),
        triangle[1].min(triangle[3]).min(triangle[5]),
        triangle[0].max(triangle[2]).max(triangle[4]),
        triangle[1].max(triangle[3]).max(triangle[5]),
    ]
}

/// The land borders as one mesh of dashes, walked along every visible piece.
///
/// A border is furniture rather than a feature of the coastline, so its width and its dash
/// stay in pixels and do not grow with the camera. Each piece anchors the pattern at its
/// own first point: the build cut the pieces per cell, so a border crossing a cell edge
/// starts a fresh dash there rather than having its phase crawl when the camera pans.
pub(super) fn border_mesh(
    layer: &LineLayer,
    projection: Projection,
    origin: [f32; 2],
    cells: CellSpan,
    dash: Dash,
) -> Mesh {
    let mut mesh = MeshBuilder::new();
    for piece in visible_pieces(layer, cells) {
        let mut previous: Option<[f32; 2]> = None;
        let mut along = 0.0f32;
        for point in piece_points(layer, piece) {
            let next = projection.screen(*point);
            if let Some(start) = previous {
                let length = ((next[0] - start[0]).powi(2) + (next[1] - start[1]).powi(2)).sqrt();
                if length > 0.0 {
                    push_dashes(&mut mesh, start, next, along, length, dash, origin);
                }
                along += length;
            }
            previous = Some(next);
        }
    }
    mesh.finish()
}

/// The roads of one frame, split into the two widths and colours they are drawn in.
///
/// One pass over the visible pieces: each piece is filtered and its path chosen by the
/// same [`model::road_path`] answer, so a piece cannot be drawn in a path its filter had
/// dropped. Mesh 0 is motorways and trunks, mesh 1 primary roads; at the fitted zoom the
/// second is empty, and [`in_paint_order`] skips it.
pub(super) fn road_meshes(
    layer: &LineLayer,
    projection: Projection,
    origin: [f32; 2],
    cells: CellSpan,
    filter: RoadFilter,
) -> [Mesh; 2] {
    let mut meshes = [MeshBuilder::new(), MeshBuilder::new()];
    let widths = [ROAD_WIDTH, ROAD_MINOR_WIDTH];
    let metres_per_pixel = projection.metres_per_pixel();
    for piece in visible_pieces(layer, cells) {
        let Some(index) = model::road_path(filter, piece, metres_per_pixel) else {
            continue;
        };
        for segment in piece_points(layer, piece).windows(2) {
            push_ribbon(
                &mut meshes[index],
                projection.screen(segment[0]),
                projection.screen(segment[1]),
                widths[index],
                origin,
            );
        }
    }
    meshes.map(MeshBuilder::finish)
}

/// Every piece the frame's cells hold, in cell order.
///
/// Both line layers were cut per cell by the build, so a piece is in exactly one cell and
/// visiting the viewport's cells visits each visible piece once. The cell table is
/// `GRID_COLS * GRID_ROWS` ranges the parser checked against the pieces.
fn visible_pieces(layer: &LineLayer, cells: CellSpan) -> impl Iterator<Item = &Piece> {
    cells.indices().flat_map(move |cell| {
        let range = &layer.cells[cell as usize];
        layer.pieces[range.start as usize..range.end as usize].iter()
    })
}

/// The points of one piece, as the slice its layer stores them in.
fn piece_points<'a>(layer: &'a LineLayer, piece: &Piece) -> &'a [[f32; 2]] {
    let start = piece.start as usize;
    &layer.points[start..start + usize::from(piece.len)]
}

/// Appends one screen-space segment as a ribbon `width` pixels wide: two triangles.
///
/// A segment is drawn as a ribbon rather than as a stroked path, so its width is in
/// pixels and does not grow with the camera. A segment with no length — the build can
/// store two points on top of each other — draws nothing rather than a degenerate
/// triangle.
fn push_ribbon(
    mesh: &mut MeshBuilder,
    start: [f32; 2],
    end: [f32; 2],
    width: f32,
    origin: [f32; 2],
) {
    let span = [end[0] - start[0], end[1] - start[1]];
    let length = (span[0] * span[0] + span[1] * span[1]).sqrt();
    if !length.is_finite() || length <= 0.0 {
        return;
    }
    let direction = [span[0] / length, span[1] / length];
    let normal = [-direction[1] * width / 2.0, direction[0] * width / 2.0];
    let side =
        |point: [f32; 2], sign: f32| [point[0] + normal[0] * sign, point[1] + normal[1] * sign];
    mesh.push_quad([
        canvas_point(origin, side(start, 1.0)),
        canvas_point(origin, side(end, 1.0)),
        canvas_point(origin, side(end, -1.0)),
        canvas_point(origin, side(start, -1.0)),
    ]);
}

/// Appends the dashes of one screen-space segment of a dashed polyline.
///
/// `along` is how far the segment starts into its piece, so the pattern is anchored at the
/// piece's own first point and the dashes of two segments join without a seam.
fn push_dashes(
    mesh: &mut MeshBuilder,
    start: [f32; 2],
    end: [f32; 2],
    along: f32,
    length: f32,
    dash: Dash,
    origin: [f32; 2],
) {
    let direction = [(end[0] - start[0]) / length, (end[1] - start[1]) / length];
    let period = dash.on + dash.off;
    let first = (along / period).floor() as i64;
    let last = ((along + length) / period).ceil() as i64;
    for cycle in first..=last {
        let from = (cycle as f32 * period).max(along);
        let to = (cycle as f32 * period + dash.on).min(along + length);
        if to <= from {
            continue;
        }
        let at = |distance: f32| {
            let travelled = distance - along;
            [
                start[0] + direction[0] * travelled,
                start[1] + direction[1] * travelled,
            ]
        };
        push_ribbon(mesh, at(from), at(to), dash.width, origin);
    }
}

/// The vessels of one frame, one mesh per ship type.
pub(super) struct ShipPaths {
    /// One mesh per type in [`SHIP_TYPES`] order. The unknown mesh holds dots; every
    /// other mesh holds marker triangles.
    pub types: [Mesh; SHIP_TYPE_COUNT],
}

/// Splits the painted vessels into the meshes one frame draws.
///
/// A vessel with no type is a grey dot rather than a coloured marker: its position is
/// still a position, so it is drawn — a map that hid it would be reporting fewer vessels
/// than the service does.
pub(super) fn ship_meshes(
    vessels: &[Vessel],
    projection: Projection,
    origin: [f32; 2],
) -> ShipPaths {
    let mut builders: [MeshBuilder; SHIP_TYPE_COUNT] = std::array::from_fn(|_| MeshBuilder::new());
    // The marker grows from ten pixels at the fitted zoom to twelve at the closest one,
    // so a hull stays visible while the sea around it gains detail.
    let zoom =
        ((projection.zoom - model::MIN_ZOOM) / (model::MAX_ZOOM - model::MIN_ZOOM)).clamp(0.0, 1.0);
    let length = MARKER_LENGTH_NEAR + (MARKER_LENGTH_FAR - MARKER_LENGTH_NEAR) * zoom;

    for vessel in vessels {
        let position = projection.screen(vessel.position);
        if !projection.near_screen(position, length) {
            continue;
        }
        let mesh = &mut builders[vessel.ship_type.index()];
        if vessel.ship_type == ShipType::Unknown {
            push_disc(
                mesh,
                position,
                UNKNOWN_VESSEL_RADIUS,
                ROUND_SEGMENTS,
                origin,
            );
            continue;
        }
        let radians = vessel.facing().to_radians();
        let (sin, cos) = radians.sin_cos();
        // North is up in canvas coordinates, so the course turns a direction whose y is
        // negated.
        let direction = [sin, -cos];
        let normal = [-direction[1], direction[0]];
        let half = length / 2.0;
        let half_width = length * MARKER_WIDTH_RATIO / 2.0;
        let tip = [
            position[0] + direction[0] * half,
            position[1] + direction[1] * half,
        ];
        let base = [
            position[0] - direction[0] * half,
            position[1] - direction[1] * half,
        ];
        let left = [
            base[0] + normal[0] * half_width,
            base[1] + normal[1] * half_width,
        ];
        let right = [
            base[0] - normal[0] * half_width,
            base[1] - normal[1] * half_width,
        ];
        mesh.push_triangle([
            canvas_point(origin, tip),
            canvas_point(origin, left),
            canvas_point(origin, right),
        ]);
    }

    ShipPaths {
        types: builders.map(MeshBuilder::finish),
    }
}

/// Appends a filled circle centred on `center`, as a fan of triangles.
fn push_disc(
    mesh: &mut MeshBuilder,
    center: [f32; 2],
    radius: f32,
    segments: usize,
    origin: [f32; 2],
) {
    let hub = canvas_point(origin, center);
    for segment in 0..segments {
        let first = segment as f32 / segments as f32 * std::f32::consts::TAU;
        let second = (segment + 1) as f32 / segments as f32 * std::f32::consts::TAU;
        let a = canvas_point(
            origin,
            [
                center[0] + first.cos() * radius,
                center[1] + first.sin() * radius,
            ],
        );
        let b = canvas_point(
            origin,
            [
                center[0] + second.cos() * radius,
                center[1] + second.sin() * radius,
            ],
        );
        mesh.push_triangle([hub, a, b]);
    }
}

/// The ring around the vessel under the pointer, as an annulus of triangles.
pub(super) fn ring_mesh(center: [f32; 2], origin: [f32; 2]) -> Mesh {
    let mut mesh = MeshBuilder::with_capacity(RING_SEGMENTS * 6, RING_SEGMENTS * 6);
    let at = |angle: f32, radius: f32| {
        canvas_point(
            origin,
            [
                center[0] + angle.cos() * radius,
                center[1] + angle.sin() * radius,
            ],
        )
    };
    for segment in 0..RING_SEGMENTS {
        let first = segment as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
        let second = (segment + 1) as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
        let inner_first = at(first, HOVER_RING_INNER);
        let inner_second = at(second, HOVER_RING_INNER);
        let outer_first = at(first, HOVER_RING_OUTER);
        let outer_second = at(second, HOVER_RING_OUTER);
        mesh.push_triangle([inner_first, outer_first, outer_second]);
        mesh.push_triangle([inner_first, outer_second, inner_second]);
    }
    mesh.finish()
}

/// One canvas point as the window point the meshes are drawn at.
///
/// An engine canvas draws in window coordinates while a projection is relative to the
/// canvas, so the box's corner is added to every vertex here.
fn canvas_point(origin: [f32; 2], canvas: [f32; 2]) -> [f32; 2] {
    [origin[0] + canvas[0], origin[1] + canvas[1]]
}

/// The base map's meshes and one ship mesh per type, in the order one frame paints them.
pub(super) struct MapLayers {
    /// The land borders, one quad per visible dash.
    pub borders: Mesh,
    /// Motorways and trunks, then primary roads.
    pub roads: [Mesh; 2],
    /// The vessels: one mesh per ship type.
    pub ships: ShipPaths,
}

/// The meshes in the order a frame paints them, each with the colour it is drawn in.
///
/// The stacking is the map's: the coast (submitted by [`crate::map`] before this list), the
/// borders over it, the roads over those, and the vessels last, so no part of the base map
/// covers a vessel. The unknown dots go down before the typed markers: a vessel whose type
/// is known is a report that says more about it, and its marker should not be hidden under a
/// neighbour's dot. A mesh with no triangle is skipped rather than submitted: the engine
/// would draw nothing for it, and this list is also the record of what the picture holds —
/// an empty layer in it would say a layer is drawn when nothing is.
pub(super) fn in_paint_order(layers: MapLayers, colors: MapColors) -> Vec<(Mesh, Color)> {
    let MapLayers {
        borders,
        roads: [motorway, primary],
        ships,
    } = layers;
    let mut types = ships.types.map(Some);
    let mut painted = Vec::with_capacity(3 + SHIP_TYPE_COUNT);

    push_layer(&mut painted, borders, colors.map_border);
    push_layer(&mut painted, motorway, colors.road);
    push_layer(&mut painted, primary, colors.road_minor);

    let unknown = ShipType::Unknown;
    if let Some(mesh) = types[unknown.index()].take() {
        push_layer(&mut painted, mesh, colors.ships[unknown.index()]);
    }
    for ship_type in SHIP_TYPES {
        if ship_type == unknown {
            continue;
        }
        if let Some(mesh) = types[ship_type.index()].take() {
            push_layer(&mut painted, mesh, colors.ships[ship_type.index()]);
        }
    }
    painted
}

/// Appends one mesh and its colour, unless the mesh draws nothing.
fn push_layer(layers: &mut Vec<(Mesh, Color)>, mesh: Mesh, color: Color) {
    if !mesh.is_empty() {
        layers.push((mesh, color));
    }
}

/// Builds the per-frame screen-space meshes (the land is static Mercator geometry and is
/// not part of this).
///
/// Pure: no window and no theme. The scale the projection draws at decides which class-0
/// road copy is drawn, the clamped zoom decides when the primary roads join it, and the
/// viewport decides which cells are read, so two frames of the same camera and the same
/// picture build the same geometry. The line layers were cut per cell by the build, so they
/// take the viewport's span ungrown — growing it would only revisit pieces the neighbouring
/// cells already drew. The land's span is the one that is grown, because it is bucketed by
/// triangle centroid; that belongs to [`visible_land_ranges`].
///
/// A projection that cannot draw — a pane laid out at zero size — builds no mesh at all: it
/// has no scale to project through, so every vertex would be the canvas' centre or a NaN,
/// and the caller's own `Projection::drawable` check is the decision, not a guard this
/// builder can make twice.
pub(super) fn frame_meshes(
    scene: Option<&'static Scene>,
    projection: Projection,
    origin: [f32; 2],
    vessels: &[Vessel],
) -> MapLayers {
    if !projection.drawable() {
        return MapLayers {
            borders: Mesh::default(),
            roads: [Mesh::default(), Mesh::default()],
            ships: ShipPaths {
                types: std::array::from_fn(|_| Mesh::default()),
            },
        };
    }

    let visible = projection.visible_mercator();
    let cells = model::visible_cells(visible);
    let (borders, roads) = match scene {
        Some(scene) => (
            border_mesh(&scene.borders, projection, origin, cells, BORDER_DASH),
            road_meshes(
                &scene.roads,
                projection,
                origin,
                cells,
                model::road_filter(projection.metres_per_pixel(), projection.zoom),
            ),
        ),
        None => (Mesh::default(), [Mesh::default(), Mesh::default()]),
    };
    MapLayers {
        borders,
        roads,
        ships: ship_meshes(vessels, projection, origin),
    }
}

#[cfg(test)]
mod tests {
    use base_ui::theme::{appearance::Appearance, resolved::Theme};

    use super::*;
    use crate::{
        model::{Camera, MAX_ZOOM},
        scene,
    };

    /// Asserts two floats are the same within the tolerance a projection needs.
    fn assert_close(left: f32, right: f32, what: &str) {
        assert!(
            (left - right).abs() < 1e-4,
            "{what}: {left} should be {right}",
        );
    }

    /// The triangles one mesh holds, with the list's invariants checked.
    ///
    /// The meshes are unshared triangle lists, so this is also the number of vertices the
    /// engine's geometry carries and the step the instance buffer used to be measured in.
    fn triangles(mesh: &Mesh) -> usize {
        assert_eq!(
            mesh.vertices.len(),
            mesh.indices.len(),
            "a mesh has one vertex per index",
        );
        assert_eq!(mesh.indices.len() % 3, 0, "every mesh is a triangle list");
        mesh.triangle_count()
    }

    /// The vertical extent a screen-space mesh covers, in pixels.
    fn quad_height(mesh: &Mesh) -> f32 {
        let (low, high) = mesh
            .vertices
            .iter()
            .map(|vertex| vertex[1])
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), y| {
                (low.min(y), high.max(y))
            });
        high - low
    }

    /// Asserts two meshes hold the same geometry, sampled at both ends and the middle.
    ///
    /// The count, the triangle list and the samples are enough to pin the mesh a builder
    /// produces, because the builders emit in a fixed order. An empty expected mesh is a
    /// filter's answer rather than a fixture that draws nothing, so only the counts are
    /// compared.
    fn assert_same_geometry(actual: &Mesh, expected: &Mesh, what: &str) {
        assert_eq!(
            actual.vertices.len(),
            expected.vertices.len(),
            "{what}: the same number of vertices",
        );
        assert_eq!(
            actual.indices, expected.indices,
            "{what}: the same triangle list",
        );
        if expected.vertices.is_empty() {
            return;
        }
        for index in [0, expected.vertices.len() / 2, expected.vertices.len() - 1] {
            assert_eq!(
                actual.vertices[index], expected.vertices[index],
                "{what}: vertex {index}",
            );
        }
    }

    /// The screen meshes are in window coordinates, while the projection and the labels are
    /// relative to the canvas box: a vertex that does not carry the box's corner draws the
    /// whole map at the window's corner, under labels laid out somewhere else.
    ///
    /// The coastline is the exception now: it is Mercator geometry built once per tier, and
    /// the frame transforms it, so its vertices carry no window position at all — a land
    /// vertex that carried the corner would be transformed twice.
    #[test]
    fn the_painted_geometry_should_be_offset_by_the_canvas_box() {
        let projection = Projection::new(
            Camera {
                center: model::project(56.0, 26.0),
                zoom: 1.0,
            },
            [600.0, 400.0],
        );
        let origin = [41.0, 67.0];

        let scene = scene::scene().expect("the checked-in asset parses");
        let visible = projection.visible_mercator();
        let cells = model::visible_cells(visible);

        // The land keeps the asset's own Mercator coordinates.
        let layer = scene.land(projection.metres_per_pixel());
        let land = land_mesh(layer);
        let [ax, ay, ..] = layer.triangles[0];
        assert_close(land.mesh.vertices[0][0], ax, "the land mesh is Mercator x");
        assert_close(land.mesh.vertices[0][1], ay, "the land mesh is Mercator y");
        let [west, south, east, north] = model::mercator_bounds();
        let bounds = land.mesh.bounds;
        assert!(
            bounds.origin.x >= west - 1e-5
                && bounds.origin.y >= south - 1e-5
                && bounds.max().x <= east + 1e-5
                && bounds.max().y <= north + 1e-5,
            "the land mesh's box is in Mercator radians, not window pixels: {bounds:?}",
        );

        // Every screen-space builder adds the corner to every vertex: the same mesh built
        // at the canvas' own origin differs by exactly the corner, box included.
        let offset_is_exact = |moved: &Mesh, plain: &Mesh, what: &str| {
            assert_eq!(
                moved.vertices.len(),
                plain.vertices.len(),
                "{what}: the same vertices",
            );
            for (shifted, base) in moved.vertices.iter().zip(&plain.vertices) {
                assert_close(shifted[0], base[0] + origin[0], what);
                assert_close(shifted[1], base[1] + origin[1], what);
            }
            assert_close(
                moved.bounds.origin.x,
                plain.bounds.origin.x + origin[0],
                what,
            );
            assert_close(
                moved.bounds.origin.y,
                plain.bounds.origin.y + origin[1],
                what,
            );
        };
        let filter = model::road_filter(projection.metres_per_pixel(), projection.zoom);
        offset_is_exact(
            &border_mesh(&scene.borders, projection, origin, cells, BORDER_DASH),
            &border_mesh(&scene.borders, projection, [0.0, 0.0], cells, BORDER_DASH),
            "the borders keep the canvas box's corner",
        );
        let roads = road_meshes(&scene.roads, projection, origin, cells, filter);
        let roads_at_origin = road_meshes(&scene.roads, projection, [0.0, 0.0], cells, filter);
        offset_is_exact(
            &roads[0],
            &roads_at_origin[0],
            "the motorways keep the canvas box's corner",
        );

        // The marker and the ring exactly, at the points the picture puts them on.
        let centre = projection.screen(model::project(56.0, 26.0));
        let vessel = Vessel {
            mmsi: 7,
            ship_type: ShipType::Tanker,
            position: model::project(56.0, 26.0),
            cog: 0,
            heading: 0,
        };
        let ships = ship_meshes(&[vessel], projection, origin);
        let moved = &ships.types[ShipType::Tanker.index()];
        let at_origin = ship_meshes(&[vessel], projection, [0.0, 0.0]);
        offset_is_exact(
            moved,
            &at_origin.types[ShipType::Tanker.index()],
            "the markers keep the canvas box's corner",
        );
        assert_close(
            moved.vertices[0][0],
            origin[0] + centre[0],
            "the marker's tip keeps the canvas box's x",
        );
        assert_close(
            moved.vertices[0][1],
            origin[1] + centre[1] - MARKER_LENGTH_NEAR / 2.0,
            "the marker points north from its hull inside the canvas box",
        );

        let ring = ring_mesh(centre, origin);
        offset_is_exact(&ring, &ring_mesh(centre, [0.0, 0.0]), "the ring");
        assert_close(
            ring.vertices[0][0],
            origin[0] + centre[0] + HOVER_RING_INNER,
            "the ring keeps the canvas box's x",
        );
        assert_close(
            ring.vertices[0][1],
            origin[1] + centre[1],
            "the ring keeps the canvas box's y",
        );
    }

    /// One vessel of each ship type, all over the strait, so every vessel mesh has
    /// something in it.
    fn one_vessel_per_path() -> Vec<Vessel> {
        let position = model::project(56.3, 26.5);
        SHIP_TYPES
            .iter()
            .enumerate()
            .map(|(index, ship_type)| Vessel {
                mmsi: index as u32 + 1,
                ship_type: *ship_type,
                position,
                cog: 0,
                heading: 0,
            })
            .collect()
    }

    /// A line layer holding one piece in the cell its first point falls in.
    ///
    /// The build cuts a piece into the cell it lies in, so a fixture that filed it
    /// elsewhere would draw only while the frame's span happened to cover that cell.
    fn one_piece_layer(points: &[[f32; 2]], flags: u8, length_m: u32) -> LineLayer {
        let mut cells = vec![0..0; (model::GRID_COLS * model::GRID_ROWS) as usize];
        let [x, y] = points[0];
        let cell = model::visible_cells([x, y, x, y]);
        cells[(cell.row_start * model::GRID_COLS + cell.col_start) as usize] = 0..1;
        LineLayer {
            points: points.to_vec(),
            pieces: vec![Piece {
                start: 0,
                len: points.len() as u16,
                flags,
                length_m,
            }],
            cells,
        }
    }

    /// The meshes are the map's stacking: the borders, the roads, then the vessels.
    ///
    /// The order lives in one place — [`in_paint_order`] — so this is the test that replaces
    /// "did the frame paint them in order": each slot is checked against the builder its
    /// name claims, and the whole sequence is checked against the colour roles in the
    /// documented order. The coastline and the ocean quad are submitted by [`crate::map`]
    /// before this list, so they are not part of it.
    #[test]
    fn the_map_layers_are_built_in_paint_order() {
        let colors = MapColors::resolve(&Theme::of(Appearance::Light));
        let scene = scene::scene().expect("the checked-in asset parses");
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let origin = [16.0, 16.0];
        let vessels = one_vessel_per_path();
        let layers = frame_meshes(Some(scene), projection, origin, &vessels);

        let visible = projection.visible_mercator();
        let cells = model::visible_cells(visible);
        let filter = model::road_filter(projection.metres_per_pixel(), projection.zoom);
        let roads = road_meshes(&scene.roads, projection, origin, cells, filter);
        let ships = ship_meshes(&vessels, projection, origin);
        assert_same_geometry(
            &layers.borders,
            &border_mesh(&scene.borders, projection, origin, cells, BORDER_DASH),
            "the borders",
        );
        assert_same_geometry(&layers.roads[0], &roads[0], "the motorways");
        assert_same_geometry(&layers.roads[1], &roads[1], "the primary roads");
        for ship_type in SHIP_TYPES {
            assert_same_geometry(
                &layers.ships.types[ship_type.index()],
                &ships.types[ship_type.index()],
                "a ship type mesh",
            );
        }
        assert!(
            layers.roads[1].is_empty(),
            "the coarse copy draws no primary road, so the second road mesh is empty",
        );

        let painted = in_paint_order(layers, colors);
        let mut expected = vec![colors.map_border, colors.road];
        expected.push(colors.ships[ShipType::Unknown.index()]);
        expected.extend(
            SHIP_TYPES
                .iter()
                .filter(|ship_type| **ship_type != ShipType::Unknown)
                .map(|ship_type| colors.ships[ship_type.index()]),
        );
        assert_eq!(
            painted.iter().map(|(_, color)| *color).collect::<Vec<_>>(),
            expected,
            "the meshes are painted borders, roads, unknown dots, then the typed markers",
        );
        assert_eq!(
            painted.len(),
            2 + SHIP_TYPE_COUNT,
            "the empty primary mesh is skipped: the borders, one road copy and every vessel type",
        );
        assert!(
            !painted[0].0.is_empty() && !painted[1].0.is_empty(),
            "the base map has geometry at the fitted zoom",
        );
        assert!(
            painted[2..].iter().all(|(mesh, _)| !mesh.is_empty()),
            "every vessel mesh has a vessel",
        );
    }

    /// A road segment is a screen-space ribbon: two triangles, as wide as it is configured,
    /// in the mesh its class names, and its width is in pixels rather than in map units.
    #[test]
    fn a_road_segment_becomes_two_triangles_at_its_configured_width() {
        // The two points share a latitude, so the segment is horizontal on screen and the
        // quad's height is exactly the width it is drawn at.
        let points = [model::project(55.0, 25.0), model::project(55.5, 25.0)];
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let origin = [0.0, 0.0];
        let cells = model::visible_cells(projection.visible_mercator());
        let motorway = one_piece_layer(&points, 0, 2_000);
        let coarse = one_piece_layer(&points, model::ROAD_COARSE, 0);
        let primary = one_piece_layer(&points, model::ROAD_PRIMARY, 2_000);

        let roads = road_meshes(&motorway, projection, origin, cells, RoadFilter::FineCopy);
        assert_eq!(triangles(&roads[0]), 2, "one segment is two triangles");
        assert!(roads[1].is_empty(), "a motorway is not a primary road");
        assert_close(
            quad_height(&roads[0]),
            ROAD_WIDTH,
            "the motorway quad is drawn as wide as it is configured",
        );

        // The coarse copy is the same width, and only the coarse filter draws it.
        assert!(
            road_meshes(&coarse, projection, origin, cells, RoadFilter::FineCopy)[0].is_empty(),
            "the fine copy's filter does not draw the coarse one",
        );
        let roads = road_meshes(&coarse, projection, origin, cells, RoadFilter::CoarseCopy);
        assert_eq!(triangles(&roads[0]), 2, "the coarse copy is a ribbon too");
        assert_close(quad_height(&roads[0]), ROAD_WIDTH, "at the same width");

        let roads = road_meshes(
            &primary,
            projection,
            origin,
            cells,
            RoadFilter::FineCopyAndPrimary,
        );
        assert_eq!(
            triangles(&roads[1]),
            2,
            "a primary road is two triangles too",
        );
        assert!(roads[0].is_empty(), "and it is not a motorway");
        assert_close(
            quad_height(&roads[1]),
            ROAD_MINOR_WIDTH,
            "the primary quad is drawn at its own width",
        );

        // The width is in screen pixels, not map units: the same piece at four times the
        // zoom is the same number of pixels wide, which is what a ribbon buys over a stroke
        // that would scale with the camera.
        let viewport = [600.0, 400.0];
        let close = Projection::new(
            Camera {
                center: points[0],
                zoom: MAX_ZOOM,
            }
            .clamped(viewport),
            viewport,
        );
        let cells = model::visible_cells(close.visible_mercator());
        let roads = road_meshes(&motorway, close, origin, cells, RoadFilter::FineCopy);
        assert_eq!(triangles(&roads[0]), 2, "the zoom does not drop the piece",);
        assert_close(
            quad_height(&roads[0]),
            ROAD_WIDTH,
            "the ribbon keeps its width when the camera zooms in",
        );
    }

    /// A border is dashed as well as violet, so that the kind of a line is readable from
    /// its pattern and not only from its hue: one six-pixel dash, a four-pixel gap after
    /// each, and the gap really is a gap.
    ///
    /// The fixture's segment is 39 px long so it ends one pixel into the last gap: a dash
    /// boundary exactly on the segment's end would turn a sub-pixel rounding of the
    /// projected length into a fifth sliver dash.
    #[test]
    fn a_border_segment_is_dashes_of_six_pixels_on_and_four_off() {
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let start = [100.0, 50.0];
        let end = [139.0, 50.0];
        let points = [projection.mercator(start), projection.mercator(end)];
        // The projection's own round trip, which is where the dash walk starts: the length
        // it measures is the segment the pattern is laid out on.
        let projected_start = projection.screen(points[0]);
        let projected_end = projection.screen(points[1]);
        let length = projected_end[0] - projected_start[0];
        assert!(
            (36.0..40.0).contains(&length),
            "the fixture's segment is {length} px, which is the four-dash case",
        );

        let layer = one_piece_layer(&points, 0, 0);
        let cells = model::visible_cells(projection.visible_mercator());
        let mesh = border_mesh(&layer, projection, [0.0, 0.0], cells, BORDER_DASH);

        // 6 px on and 4 px off: [0, 6], [10, 16], [20, 26] and [30, 36] are drawn, and
        // [36, 40] is the last gap.
        assert_eq!(triangles(&mesh), 8, "four dashes, two triangles each");
        assert_close(
            quad_height(&mesh),
            BORDER_DASH.width,
            "a border dash keeps its width",
        );
        let (low, high) = mesh
            .vertices
            .iter()
            .map(|vertex| vertex[0])
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), x| {
                (low.min(x), high.max(x))
            });
        assert_close(
            low,
            projected_start[0],
            "the first dash starts where the piece starts",
        );
        assert_close(
            high,
            projected_start[0] + 36.0,
            "the last dash ends 36 px along the segment",
        );
        assert!(
            high < projected_start[0] + length,
            "the gap after the last dash is a gap: the mesh stops {} px short",
            projected_start[0] + length - high,
        );
    }

    /// A pane whose asset failed has no base map, but the picture is still drawn: a frame
    /// does not need the coastline to put something on screen.
    #[test]
    fn a_map_without_an_asset_still_draws_the_ships() {
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let vessels = one_vessel_per_path();
        let layers = frame_meshes(None, projection, [0.0, 0.0], &vessels);

        assert!(layers.borders.is_empty(), "there are no borders");
        assert!(
            layers.roads[0].is_empty() && layers.roads[1].is_empty(),
            "and no roads",
        );
        assert!(
            layers.ships.types.iter().all(|mesh| !mesh.is_empty()),
            "every vessel mesh has a vessel",
        );
    }

    /// A pane with no size has no scale to project through, so the frame builds nothing
    /// rather than a mesh of the canvas' centre or a NaN box.
    #[test]
    fn a_projection_that_cannot_draw_builds_no_mesh() {
        let projection = Projection::new(Camera::default(), [0.0, 0.0]);
        let scene = scene::scene().expect("the checked-in asset parses");
        let layers = frame_meshes(Some(scene), projection, [0.0, 0.0], &one_vessel_per_path());

        assert!(!projection.drawable(), "a zero viewport has no fit");
        assert!(layers.borders.is_empty(), "no borders");
        assert!(layers.roads.iter().all(Mesh::is_empty), "no roads");
        assert!(
            layers.ships.types.iter().all(Mesh::is_empty),
            "and no vessels, even though the picture has some",
        );
    }

    /// A zero-length segment draws nothing: the build can store a road's two points on top
    /// of each other, and a degenerate triangle is not a triangle a frame should submit.
    #[test]
    fn a_segment_with_no_length_draws_nothing() {
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let cells = model::visible_cells(projection.visible_mercator());
        let point = model::project(56.0, 26.0);
        let layer = one_piece_layer(&[point, point], 0, 0);

        let roads = road_meshes(&layer, projection, [0.0, 0.0], cells, RoadFilter::FineCopy);
        assert!(
            roads[0].is_empty() && roads[1].is_empty(),
            "a road segment of no length draws no quad",
        );
        let borders = border_mesh(&layer, projection, [0.0, 0.0], cells, BORDER_DASH);
        assert!(borders.is_empty(), "and a border segment of none no dash");

        let empty = Mesh::default();
        assert!(empty.is_empty());
        assert_eq!(empty.triangle_count(), 0);
        assert_eq!(empty.bounds, Rect::default(), "an empty mesh has no box");
    }

    /// A marker points where its course says and its hull is as wide as the ratio; a vessel
    /// with no type is a six-segment dot; and the marker grows to twelve pixels at the
    /// closest zoom.
    #[test]
    fn a_marker_turns_with_its_course_and_grows_with_the_zoom() {
        let viewport = [600.0, 400.0];
        let projection = Projection::new(Camera::default().clamped(viewport), viewport);
        let position = model::project(56.0, 26.0);
        let centre = projection.screen(position);
        let origin = [10.0, 20.0];
        let vessel = |ship_type: ShipType, cog: u32| Vessel {
            mmsi: 1,
            ship_type,
            position,
            cog,
            heading: 0,
        };

        let ships = ship_meshes(
            &[vessel(ShipType::Tanker, 0), vessel(ShipType::Unknown, 0)],
            projection,
            origin,
        );
        let north = &ships.types[ShipType::Tanker.index()];
        assert_eq!(triangles(north), 1, "a typed vessel is one triangle");
        assert_close(
            north.vertices[0][1],
            origin[1] + centre[1] - MARKER_LENGTH_NEAR / 2.0,
            "a north-facing marker's tip is half its length above its hull",
        );

        // A course over ground of 900 is 90°: the marker turns to the east, and the two
        // base vertices stay one marker-width apart.
        let east = ship_meshes(&[vessel(ShipType::Tanker, 900)], projection, origin);
        let east = &east.types[ShipType::Tanker.index()];
        assert_close(
            east.vertices[0][0],
            origin[0] + centre[0] + MARKER_LENGTH_NEAR / 2.0,
            "an east-facing marker's tip is half its length east of its hull",
        );
        assert_close(
            east.vertices[0][1],
            origin[1] + centre[1],
            "and it points along the parallel",
        );
        let [left, right] = [east.vertices[1], east.vertices[2]];
        let width = ((left[0] - right[0]).powi(2) + (left[1] - right[1]).powi(2)).sqrt();
        assert_close(
            width,
            MARKER_LENGTH_NEAR * MARKER_WIDTH_RATIO,
            "the hull is drawn at the marker's width ratio",
        );

        // A vessel whose ship record has not arrived is a dot: six segments, drawn at the
        // unknown radius.
        let dot = &ships.types[ShipType::Unknown.index()];
        assert_eq!(triangles(dot), ROUND_SEGMENTS, "the dot is a fan of six");
        let dot_bounds = dot.bounds;
        assert_close(
            dot_bounds.size.width,
            UNKNOWN_VESSEL_RADIUS * 2.0,
            "the dot is drawn at its radius",
        );
        assert_close(
            dot_bounds.origin.y + dot_bounds.size.height / 2.0,
            origin[1] + centre[1],
            "the dot is centred on the hull",
        );

        // Twelve pixels at the closest zoom rather than ten.
        let close = Projection::new(
            Camera {
                center: position,
                zoom: MAX_ZOOM,
            }
            .clamped(viewport),
            viewport,
        );
        let grown = ship_meshes(&[vessel(ShipType::Tanker, 0)], close, origin);
        let tip = grown.types[ShipType::Tanker.index()].vertices[0];
        let base = (grown.types[ShipType::Tanker.index()].vertices[1][1]
            + grown.types[ShipType::Tanker.index()].vertices[2][1])
            / 2.0;
        assert_close(
            base - tip[1],
            MARKER_LENGTH_FAR,
            "the marker is twelve pixels long at the closest zoom",
        );
    }

    /// The hover ring is an annulus: twenty-four quads between the two radii, so the vessel
    /// under the pointer stays readable through its middle.
    #[test]
    fn the_hover_ring_is_an_annulus_of_twenty_four_quads() {
        let centre = [100.0, 80.0];
        let origin = [7.0, 11.0];
        let ring = ring_mesh(centre, origin);

        assert_eq!(
            triangles(&ring),
            RING_SEGMENTS * 2,
            "two triangles per segment"
        );
        let middle = [origin[0] + centre[0], origin[1] + centre[1]];
        for vertex in &ring.vertices {
            let distance =
                ((vertex[0] - middle[0]).powi(2) + (vertex[1] - middle[1]).powi(2)).sqrt();
            assert!(
                (HOVER_RING_INNER - 1e-3..=HOVER_RING_OUTER + 1e-3).contains(&distance),
                "every ring vertex is on one of the two radii, not at {distance}",
            );
        }
        assert_close(
            ring.bounds.size.width,
            HOVER_RING_OUTER * 2.0,
            "the ring is as wide as its outer diameter",
        );
    }

    /// The cell table is a partition of the pieces: a span over the whole grid visits every
    /// piece once, in the table's order, and a span off the map visits none.
    #[test]
    fn the_cell_table_visits_every_piece_once() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let full = CellSpan {
            col_start: 0,
            col_end: model::GRID_COLS - 1,
            row_start: 0,
            row_end: model::GRID_ROWS - 1,
        };
        for layer in [&scene.borders, &scene.roads] {
            let visited = visible_pieces(layer, full).collect::<Vec<_>>();
            assert_eq!(visited.len(), layer.pieces.len(), "every piece is visited");
            for (index, piece) in visited.iter().enumerate() {
                assert!(
                    std::ptr::eq(*piece, &layer.pieces[index]),
                    "piece {index} is visited once, in the table's order",
                );
            }
            let off_map = model::visible_cells([-10.0, -10.0, -9.0, -9.0]);
            assert_eq!(
                visible_pieces(layer, off_map).count(),
                0,
                "a viewport off the map draws nothing",
            );
        }
    }

    /// The triangles of each tier the checked-in asset tessellates into.
    ///
    /// The land is a static mesh now, so this is the count a frame's coastline costs the
    /// GPU when it draws the whole tier: the mesh must hold every triangle, and the number
    /// is pinned to the same value [`crate::scene`]'s own tests pin, because an asset or
    /// tessellation change that moved it would move the coastline.
    const LAND_TIER_TRIANGLES: [usize; model::LAND_TIERS] = [22_876, 13_347, 9_736, 5_744];

    /// The `[west, south, east, north]` box a Mercator triangle covers.
    fn land_triangle_bounds(triangle: &[f32; 6]) -> [f32; 4] {
        [
            triangle[0].min(triangle[2]).min(triangle[4]),
            triangle[1].min(triangle[3]).min(triangle[5]),
            triangle[0].max(triangle[2]).max(triangle[4]),
            triangle[1].max(triangle[3]).max(triangle[5]),
        ]
    }

    /// The triangles a frame draws from the static land: every bucket of the cells it
    /// visits, plus the whole wide range when any wide triangle reaches the viewport.
    fn expected_land_triangles(
        layer: &LandLayer,
        land: &LandMesh,
        cells: CellSpan,
        visible: [f32; 4],
    ) -> usize {
        let celled = cells
            .indices()
            .map(|cell| {
                let bucket = &land.cell_indices[cell as usize];
                (bucket.end - bucket.start) as usize / 3
            })
            .sum::<usize>();
        let wide = if wide_reaches(layer, visible) {
            (land.wide_indices.end - land.wide_indices.start) as usize / 3
        } else {
            0
        };
        celled + wide
    }

    /// Whether any wide triangle's box reaches `visible`.
    fn wide_reaches(layer: &LandLayer, visible: [f32; 4]) -> bool {
        layer.wide.iter().any(|triangle| {
            let [west, south, east, north] = land_triangle_bounds(triangle);
            west <= visible[2] && east >= visible[0] && south <= visible[3] && north >= visible[1]
        })
    }

    /// The static land mesh holds every triangle of its tier, in the asset's cell order and
    /// then the wide ones, and nothing else.
    ///
    /// The land is drawn by index range, so a triangle the mesh lost would be a hole in the
    /// coastline rather than a slow frame; the ranges are what the frame selects, so their
    /// partition of the buffer is checked here rather than in a rendered picture.
    #[test]
    fn the_land_mesh_holds_every_tier_triangle_in_cell_order() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let [west, south, east, north] = model::mercator_bounds();
        for (tier, layer) in scene.land.iter().enumerate() {
            let land = land_mesh(layer);
            assert_eq!(
                triangles(&land.mesh),
                LAND_TIER_TRIANGLES[tier],
                "tier {tier} holds every triangle the asset tessellated",
            );
            assert_eq!(
                triangles(&land.mesh),
                layer.triangles.len() + layer.wide.len(),
                "tier {tier} is the bucketed triangles followed by the wide ones",
            );
            assert_eq!(
                land.cell_indices.len(),
                (model::GRID_COLS * model::GRID_ROWS) as usize,
                "tier {tier} keeps a bucket per grid cell",
            );

            let mut cursor = 0;
            for bucket in &land.cell_indices {
                assert_eq!(
                    bucket.start as usize, cursor,
                    "tier {tier}: the buckets are contiguous and in cell order",
                );
                cursor = bucket.end as usize;
            }
            assert_eq!(
                cursor,
                layer.triangles.len() * 3,
                "tier {tier}: the buckets cover exactly the bucketed triangles",
            );
            assert_eq!(
                land.wide_indices,
                (layer.triangles.len() as u32 * 3)..(land.mesh.indices.len() as u32),
                "tier {tier}: the wide range is the mesh's tail",
            );

            let bounds = land.mesh.bounds;
            assert_close(
                bounds.origin.x,
                west,
                &format!("tier {tier}: the land starts at the map's west edge"),
            );
            assert_close(
                bounds.origin.y,
                south,
                &format!("tier {tier}: and at its south edge"),
            );
            assert_close(
                bounds.max().x,
                east,
                &format!("tier {tier}: and ends at its east edge"),
            );
            assert_close(
                bounds.max().y,
                north,
                &format!("tier {tier}: and at its north edge"),
            );
        }
    }

    /// The ranges a frame selects are the visible cells' buckets — one range, because the
    /// buckets are contiguous — plus the wide range when one of those triangles reaches the
    /// viewport.
    #[test]
    fn the_visible_land_ranges_select_the_visible_cells_and_the_wide_triangles() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let projection =
            Projection::new(Camera::default().clamped(DESIGN_VIEWPORT), DESIGN_VIEWPORT);
        let layer = scene.land(projection.metres_per_pixel());
        let land = land_mesh(layer);
        let visible = projection.visible_mercator();
        let cells = model::visible_cells(visible).grown();
        let ranges = visible_land_ranges(&land, layer, cells, visible);

        let drawn: usize = ranges
            .iter()
            .map(|range| (range.end - range.start) as usize / 3)
            .sum();
        assert_eq!(
            drawn,
            expected_land_triangles(layer, &land, cells, visible),
            "the frame draws the buckets of the cells it visits, plus the wide triangles",
        );
        assert!(drawn > 0, "the fitted frame sees the coast");

        // The cells the frame visits are one range: from the first bucket that holds a
        // triangle to the last one, because the buckets are contiguous and the empty ones
        // take no index. The wide triangles, when they reach the viewport, are the other.
        let filled = cells
            .indices()
            .map(|cell| &land.cell_indices[cell as usize])
            .filter(|bucket| bucket.start != bucket.end)
            .collect::<Vec<_>>();
        let first = filled.first().expect("the fitted frame's cells hold land");
        let last = filled.last().expect("the fitted frame's cells hold land");
        let mut expected_ranges: Vec<Range<u32>> = Vec::new();
        expected_ranges.push(first.start..last.end);
        if wide_reaches(layer, visible) {
            expected_ranges.push(land.wide_indices.start..land.wide_indices.end);
        }
        assert_eq!(
            ranges, expected_ranges,
            "a frame submits the cells' buckets as one range, plus the wide ones when they \
             reach the viewport",
        );

        // A viewport off the map selects nothing at all.
        let off_map = model::visible_cells([-10.0, -10.0, -9.0, -9.0]);
        assert!(
            visible_land_ranges(&land, layer, off_map.grown(), [-10.0, -10.0, -9.0, -9.0])
                .is_empty(),
            "a viewport off the map draws no land",
        );
    }

    /// The most triangles one frame may draw and still hold 60 fps: 8.58 ms of fixed cost
    /// plus 107 ns per triangle has to stay inside the 16.65 ms a 60 Hz frame has.
    const FRAME_TRIANGLE_BUDGET: usize = 75_000;

    /// The class-0 pieces the coarse copy holds, as the checked-in asset stores them.
    ///
    /// The coarse copy is the chains at least the build's 0.3 km feature-dropping
    /// threshold long; every one of them is drawn at the fitted zoom. The short chains
    /// are absent here and come back through the fine copy at 1.6× on the design pane.
    const CLASS_ZERO_COARSE_PIECES: usize = 4_288;

    /// The class-0 pieces the middle copy holds: the same long chains at 0.004°.
    const CLASS_ZERO_MIDDLE_PIECES: usize = 4_389;

    /// The class-0 pieces the fine copy holds: every chain, including the short ones the
    /// runtime gates below [`model::ROAD_SHORT_CHAIN_METRES_PER_PIXEL`].
    const CLASS_ZERO_FINE_PIECES: usize = 7_728;

    /// The class-0 records of all three copies. Not a partition of the roads: the middle
    /// and coarse copies are second and third geometries of the long chains the fine copy
    /// also holds, so the counts overlap in what they represent even though no record is
    /// in more than one copy.
    const CLASS_ZERO_PIECES: usize =
        CLASS_ZERO_COARSE_PIECES + CLASS_ZERO_MIDDLE_PIECES + CLASS_ZERO_FINE_PIECES;

    /// The plan's §8 targets for the fitted frame: triangles and vertices.
    ///
    /// Both are met by [`FITTED_FRAME_TRIANGLES`]: 20,659 triangles and 61,977 vertices
    /// against 21,000 and 65,536 (341 and 3,559 to spare). The vertex target is the
    /// 2^16 instance-buffer step the plan's §1.1 explains.
    const FITTED_FRAME_TARGET_TRIANGLES: usize = 21_000;
    const FITTED_FRAME_TARGET_VERTICES: usize = 65_536;

    /// The plan's §8 target for the worst frame, in triangles.
    ///
    /// **Not met.** Its successor added the 0.002° land tier, the six-segment vessel dots
    /// and a 0.004° middle road copy; the worst frame fell from 39,438 to 31,815, still
    /// 4,815 over. See [`WORST_FRAME_TRIANGLES`] for the measured frame and the layer that
    /// owns the gap.
    const WORST_FRAME_TARGET_TRIANGLES: usize = 27_000;

    /// The worst frame the viewport and zoom sweep found, in triangles, before the port.
    ///
    /// Pinned so an asset or palette change that makes a view heavier has to be measured
    /// again rather than absorbed silently into the budget. The busiest view was the
    /// **2,048 px wide pane at 1.8648× over 25.879N, 52.697E**: land 10,721 (the 0.002°
    /// tier), borders 850, fine roads 13,916, the full vessel picture 15,096 and the ring
    /// 48 — **40,631 in all, 13,631 over the plan's 27,000**. The vessel layer grew from
    /// 6,000 to 15,096 when the picture stopped being capped at 1,000 vessels.
    ///
    /// The frame sits just below the 0.004° middle copy's 371.1 m/px switch: at 369 m/px
    /// the fine road copy is drawn (13,916), while the middle copy is the next band up.
    /// It is the road LOD cliff, not a land threshold. The sweep's 7 × 5 centre grid plus
    /// two 9 × 9 refinement rounds found it. The next lever is a finer middle road copy
    /// (0.003°) for the 278–371 m/px band; that is outside this port's scope, and the
    /// number is kept as the bound the dynamic layers are asserted against (the coast it
    /// also counted is a static mesh now).
    const WORST_FRAME_TRIANGLES: usize = 40_631;

    /// The triangles the fitted frame drew before the port, coastline included.
    ///
    /// The paths of [`MapLayers`]'s predecessor with an empty picture and no hover ring:
    /// the coarsest coast, the dashed borders and the coarse copy of the long road chains.
    /// 5,733 + 2,312 + 12,614 = 20,659, inside both §8 targets.
    const FITTED_FRAME_TRIANGLES: usize = 20_659;

    /// The fitted frame's coastline: the coarsest tier's triangles it drew.
    ///
    /// The land is a static Mercator mesh now, so a frame no longer builds these triangles
    /// on the CPU: the constants below keep the historical totals, and the dynamic pins are
    /// their difference from them.
    const FITTED_FRAME_LAND_TRIANGLES: usize = 5_733;

    /// The triangles the fitted frame drew with the typical picture: 2,516 vessels whose
    /// ship record has arrived, one marker triangle each.
    ///
    /// Pinned separately from [`FITTED_FRAME_TRIANGLES`] because the empty frame's 21,000
    /// target does not cover it: the vessel layer adds 2,516 triangles, and the frame's
    /// gate is the instance-buffer step instead (see
    /// [`FITTED_FRAME_TYPICAL_TARGET_TRIANGLES`]). It was 23,175 triangles / 69,525
    /// vertices, so the step moves from 2^16 to 2^17.
    const FITTED_FRAME_TYPICAL_TRIANGLES: usize = 23_175;

    /// The 2^17 instance-buffer step in triangles: `131,072 / 3` rounded down.
    ///
    /// The typical fitted frame is allowed to sit in the 2^17 step — the picture holds
    /// the whole map's 2,516 vessels, which no longer fits the 2^16 step — while the
    /// empty frame keeps the stricter §8 21,000 target.
    const FITTED_FRAME_TYPICAL_TARGET_TRIANGLES: usize = 131_072 / 3;

    /// The triangles the 2.8× design frame drew with a full picture, before the port.
    ///
    /// Pinned separately because it is the frame the plan's worst-case analysis names.
    /// It drew the 0.002° coast (7,122) and the fine road copy (8,740):
    /// 7,122 + 784 + 8,740 + 15,096 + 48 = 31,790, over the 27,000 target by 4,790 because
    /// the whole map's 2,516 vessels are the full picture.
    const REFERENCE_FRAME_2_8_TRIANGLES: usize = 31_790;

    /// The 2.8× design frame's coastline, which the dynamic pins subtract.
    const REFERENCE_FRAME_2_8_LAND_TRIANGLES: usize = 7_122;

    /// One frame's triangles, layer by layer, without the static land.
    ///
    /// The budget is a sum, so when a frame goes over it the question is always *which*
    /// layer did: this keeps the road layer split into the three copies the filter
    /// chooses between and the primary roads it may add, and every assertion that fails
    /// prints the whole breakdown rather than only the total. The coastline is not a field:
    /// it is drawn from a static Mercator mesh, so it costs a frame the index ranges
    /// [`visible_land_ranges`] selects and no CPU triangles at all — [`LayerTriangles`] is
    /// what a frame builds, and the coast's own cost is pinned by
    /// [`LAND_TIER_TRIANGLES`].
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct LayerTriangles {
        /// The dashed land borders.
        borders: usize,
        /// The coarse copy of class 0, drawn while the scale is coarse.
        roads_coarse: usize,
        /// The middle copy of class 0, drawn between the coarse and fine copies.
        roads_middle: usize,
        /// The fine copy of class 0, drawn once the middle copy is too coarse.
        roads_fine: usize,
        /// The primary roads, drawn only at the closest zooms.
        roads_primary: usize,
        /// The vessel layer.
        ships: usize,
        /// The hover ring, which is outside [`MapLayers`].
        ring: usize,
    }

    impl LayerTriangles {
        /// The triangles `layers` builds under `filter`, plus the hover ring.
        ///
        /// The ring is drawn only when a vessel is hovered; counting it always is the
        /// worst case. The labels, the legend, the card and the canvas' own sea fill are
        /// elements rather than mesh triangles, and the plan's 8.58 ms intercept already
        /// carries them, which is why they are not counted here. The vessels are part of
        /// the map's meshes, so nothing is added twice.
        fn of(layers: &MapLayers, ring: &Mesh, filter: RoadFilter) -> Self {
            let [class_zero, primary] = &layers.roads;
            let (roads_coarse, roads_middle, roads_fine) = match filter {
                RoadFilter::CoarseCopy => (triangles(class_zero), 0, 0),
                RoadFilter::MiddleCopy | RoadFilter::MiddleCopyAndPrimary => {
                    (0, triangles(class_zero), 0)
                }
                RoadFilter::FineCopy | RoadFilter::FineCopyAndPrimary => {
                    (0, 0, triangles(class_zero))
                }
            };
            Self {
                borders: triangles(&layers.borders),
                roads_coarse,
                roads_middle,
                roads_fine,
                roads_primary: triangles(primary),
                ships: layers.ships.types.iter().map(triangles).sum::<usize>(),
                ring: triangles(ring),
            }
        }

        /// The triangles of all layers a frame builds.
        fn total(self) -> usize {
            self.borders
                + self.roads_coarse
                + self.roads_middle
                + self.roads_fine
                + self.roads_primary
                + self.ships
                + self.ring
        }

        /// The road layer's triangles: the copy in use plus the primary roads.
        fn roads(self) -> usize {
            self.roads_coarse + self.roads_middle + self.roads_fine + self.roads_primary
        }

        /// The breakdown as one line, for an assertion that has to say where the budget
        /// went.
        fn describe(self) -> String {
            format!(
                "borders {}, roads {} (coarse {}, middle {}, fine {}, primary {}), \
                 ships {}, ring {} = {} triangles",
                self.borders,
                self.roads(),
                self.roads_coarse,
                self.roads_middle,
                self.roads_fine,
                self.roads_primary,
                self.ships,
                self.ring,
                self.total(),
            )
        }
    }

    /// The whole map's vessel count, measured against the source's full response.
    const PICTURE_VESSELS: usize = 2_516;
    /// The grid the picture's vessels are spread over: 64 × 40 = 2,560 cells, enough
    /// for every vessel to be inside the viewport at once.
    const PICTURE_COLUMNS: usize = 64;
    const PICTURE_ROWS: usize = 40;

    /// One picture, with every vessel at its most expensive.
    ///
    /// A vessel with no type is a disc of [`ROUND_SEGMENTS`] triangles; a vessel with one
    /// is a single marker triangle. Spreading `PICTURE_VESSELS` untyped vessels over the
    /// viewport therefore counts the most the vessel layer can ever draw — no allowance,
    /// and no assumption about how many records have arrived.
    fn a_full_picture(projection: Projection) -> Vec<Vessel> {
        (0..PICTURE_VESSELS)
            .map(|index| {
                let column = (index % PICTURE_COLUMNS) as f32;
                let row = (index / PICTURE_COLUMNS) as f32;
                let canvas = [
                    projection.viewport[0] * (column + 0.5) / PICTURE_COLUMNS as f32,
                    projection.viewport[1] * (row + 0.5) / PICTURE_ROWS as f32,
                ];
                Vessel {
                    mmsi: index as u32 + 1,
                    ship_type: ShipType::Unknown,
                    position: projection.mercator(canvas),
                    cog: 0,
                    heading: 0,
                }
            })
            .collect()
    }

    /// One picture, with every vessel's ship record arrived.
    ///
    /// A vessel with a ship record is one marker triangle, which is the typical frame;
    /// [`a_full_picture`] is the worst one (a six-segment dot per vessel).
    fn a_typical_picture(projection: Projection) -> Vec<Vessel> {
        (0..PICTURE_VESSELS)
            .map(|index| {
                let column = (index % PICTURE_COLUMNS) as f32;
                let row = (index / PICTURE_COLUMNS) as f32;
                let canvas = [
                    projection.viewport[0] * (column + 0.5) / PICTURE_COLUMNS as f32,
                    projection.viewport[1] * (row + 0.5) / PICTURE_ROWS as f32,
                ];
                Vessel {
                    mmsi: index as u32 + 1,
                    ship_type: ShipType::Tanker,
                    position: projection.mercator(canvas),
                    cog: 0,
                    heading: 0,
                }
            })
            .collect()
    }

    /// The number of latitude samples in the sweep's coarse centre grid.
    const SWEEP_LATITUDES: usize = 7;
    /// The number of longitude samples in the sweep's coarse centre grid.
    const SWEEP_LONGITUDES: usize = 5;
    /// How many of the heaviest coarse frames are refined.
    const SWEEP_REFINEMENTS: usize = 20;
    /// The samples per axis in each refinement round.
    const SWEEP_REFINEMENT: usize = 9;
    /// How many refinement rounds each of those frames gets.
    const SWEEP_REFINEMENT_ROUNDS: usize = 2;

    /// The camera's clamped centre range at `viewport` and `zoom`, as `[x0, x1, y0, y1]`.
    ///
    /// The camera may pan until the map's edge reaches the viewport, so the centre is
    /// confined to the map grown in by half a viewport. An axis whose half-extent does
    /// not fit (a pane whose aspect matches the map's) has one point, its centre.
    fn centre_range(viewport: [f32; 2], zoom: f32) -> [f32; 4] {
        let [west, south, east, north] = model::mercator_bounds();
        let scale = model::fit_px_per_merc(viewport) * zoom;
        if !scale.is_finite() || scale <= 0.0 {
            let [x, y] = Camera::default().center;
            return [x, x, y, y];
        }
        let half = [viewport[0] / (2.0 * scale), viewport[1] / (2.0 * scale)];
        let x = if west + half[0] >= east - half[0] {
            let centre = (west + east) / 2.0;
            (centre, centre)
        } else {
            (west + half[0], east - half[0])
        };
        let y = if south + half[1] >= north - half[1] {
            let centre = (south + north) / 2.0;
            (centre, centre)
        } else {
            (south + half[1], north - half[1])
        };
        [x.0, x.1, y.0, y.1]
    }

    /// A `rows × cols` grid of camera centres over `range`, each with its label.
    ///
    /// The samples include both ends of the range, which is where the camera can be
    /// left after a drag. A degenerate axis collapses to its one point, so a pane whose
    /// viewport already spans the map samples only the centres it can reach.
    fn sample_centres(range: [f32; 4], rows: usize, cols: usize) -> Vec<([f32; 2], String)> {
        let mut centres = Vec::with_capacity(rows * cols);
        for row in 0..rows {
            let ty = if rows <= 1 {
                0.5
            } else {
                row as f32 / (rows - 1) as f32
            };
            let y = range[2] + (range[3] - range[2]) * ty;
            for col in 0..cols {
                let tx = if cols <= 1 {
                    0.5
                } else {
                    col as f32 / (cols - 1) as f32
                };
                let x = range[0] + (range[1] - range[0]) * tx;
                centres.push((
                    [x, y],
                    format!("{:.3}N {:.3}E", model::latitude_of(y), x.to_degrees()),
                ));
            }
        }
        centres.sort_by(|left, right| {
            left.0[0]
                .total_cmp(&right.0[0])
                .then(left.0[1].total_cmp(&right.0[1]))
        });
        centres.dedup_by(|left, right| left.0 == right.0);
        centres
    }

    /// One frame the sweep measured, with the camera that produced it.
    #[derive(Clone, Copy, Debug)]
    struct SweptFrame {
        /// Which of the sweep's panes this frame was drawn in.
        pane: &'static str,
        /// The pane's size, in pixels.
        viewport: [f32; 2],
        /// The camera's centre, in Mercator radians.
        center: [f32; 2],
        /// The camera's zoom.
        zoom: f32,
        /// The layer-by-layer triangle count.
        triangles: LayerTriangles,
    }

    /// One frame has to fit a 60 Hz budget in the worst view the camera can be left in.
    ///
    /// The totals are the triangles a frame builds: every mesh of [`MapLayers`] plus the
    /// hover ring, with a full picture of [`PICTURE_VESSELS`] unknown vessels — the worst
    /// each vessel can cost. Nothing is set aside as an allowance. The coastline is not in
    /// the total: it is a static Mercator mesh drawn by index range, so the triangles it
    /// adds to the GPU's frame are the ones the selection covers, not ones a frame builds
    /// (see [`WORST_FRAME_TRIANGLES`] for the pre-port frame that counted them, and the
    /// static pin below for the coast's own cost).
    ///
    /// The fit covers the viewport, so 1× does not show the whole map. The busiest view is
    /// not the fitted one: the coast and the road copy both get heavier as the camera
    /// zooms in, and the peak sits just past a switch. The sweep below covers the user's
    /// 1.56:1 pane, 1,600 and 2,048 px wide panes and a tall one, at a dense zoom walk plus
    /// the exact zooms every switch happens at, and at a **7 × 5 grid of camera centres over
    /// the camera's clamp range**, refined by two 9 × 9 rounds around the twenty heaviest
    /// coarse frames. That is a grid sample, not an analytic upper bound: a view between
    /// two samples can be heavier by whatever the coastline changes between them, which is
    /// why the next lever is measured, not assumed.
    #[test]
    fn the_visible_triangle_budget_is_under_the_frame_budget() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let origin = [0.0, 0.0];
        let ring = ring_mesh([100.0, 100.0], origin);
        let no_ring = Mesh::default();
        let at = |viewport: [f32; 2], zoom: f32, center: [f32; 2]| {
            Projection::new(Camera { center, zoom }.clamped(viewport), viewport)
        };
        let breakdown = |projection: Projection, vessels: &[Vessel], ring: &Mesh| {
            let layers = frame_meshes(Some(scene), projection, origin, vessels);
            LayerTriangles::of(
                &layers,
                ring,
                model::road_filter(projection.metres_per_pixel(), projection.zoom),
            )
        };

        // The two frames the plan's §8 targets name, printed layer by layer: the fitted
        // frame the 21,000 target is about, and the 2.8× frame the old worst case sat in.
        let fitted_projection = at(DESIGN_VIEWPORT, model::MIN_ZOOM, Camera::default().center);
        let fitted = breakdown(fitted_projection, &[], &no_ring);
        let typical = breakdown(
            fitted_projection,
            &a_typical_picture(fitted_projection),
            &no_ring,
        );
        let reference_projection = at(DESIGN_VIEWPORT, 2.8, Camera::default().center);
        let reference = breakdown(
            reference_projection,
            &a_full_picture(reference_projection),
            &ring,
        );
        // The wide pane's coarse→fine switch, which was the worst frame before the
        // primary layer moved to the closest zoom; both sides are printed so the
        // short-chain dropping's effect on it stays measured.
        let wide_viewport = [1_600.0, 866.0];
        let wide_switch = Projection::new(Camera::default(), wide_viewport).metres_per_pixel()
            / model::ROAD_COARSE_METRES_PER_PIXEL;
        let wide_at = |zoom: f32| {
            let projection = at(wide_viewport, zoom, Camera::default().center);
            breakdown(projection, &a_full_picture(projection), &ring)
        };
        let at_zoom = |zoom: f32| {
            let projection = at(DESIGN_VIEWPORT, zoom, Camera::default().center);
            breakdown(projection, &a_full_picture(projection), &ring)
        };
        println!("1× fitted frame, no vessels: {}", fitted.describe());
        println!(
            "1× fitted frame, {PICTURE_VESSELS} single-triangle vessels: {}",
            typical.describe()
        );
        println!("2.8× design frame, full picture: {}", reference.describe());
        println!(
            "3.49× design frame, full picture: {}",
            at_zoom(3.49).describe()
        );
        println!(
            "4× design frame, full picture: {}",
            at_zoom(MAX_ZOOM).describe()
        );
        println!(
            "wide pane at {:.3}× (coarse side), full picture: {}",
            wide_switch - 0.01,
            wide_at(wide_switch - 0.01).describe()
        );
        println!(
            "wide pane at {:.3}× (fine side), full picture: {}",
            wide_switch + 0.01,
            wide_at(wide_switch + 0.01).describe()
        );
        println!(
            "wide pane at 1.309×, full picture: {}",
            wide_at(1.309).describe()
        );
        println!(
            "wide pane at 1.329×, full picture: {}",
            wide_at(1.329).describe()
        );
        let wide_2048_viewport = [2_048.0, 900.0];
        let wide_2048_projection = at(
            wide_2048_viewport,
            model::MIN_ZOOM,
            Camera::default().center,
        );
        println!(
            "2,048 pane at 1×, full picture: {}",
            breakdown(
                wide_2048_projection,
                &a_full_picture(wide_2048_projection),
                &ring
            )
            .describe()
        );
        for zoom in [1.1, 1.4, 1.539, 1.6, 2.0, 2.3, 2.6] {
            let projection = at(wide_2048_viewport, zoom, Camera::default().center);
            println!(
                "2,048 pane at {zoom}× over the middle, full picture: {}",
                breakdown(projection, &a_full_picture(projection), &ring).describe()
            );
        }
        assert_eq!(
            reference.total(),
            REFERENCE_FRAME_2_8_TRIANGLES - REFERENCE_FRAME_2_8_LAND_TRIANGLES,
            "the 2.8× design frame's dynamic layers moved: {}",
            reference.describe(),
        );

        let viewports = [
            ("1.56", [1_404.0, 900.0]),
            ("wide", [1_600.0, 866.0]),
            ("wide-2048", [2_048.0, 900.0]),
            ("tall", [900.0, 1_400.0]),
        ];

        let measure = |pane: &'static str, viewport: [f32; 2], zoom: f32, center: [f32; 2]| {
            let projection = at(viewport, zoom, center);
            let vessels = a_full_picture(projection);
            let layers = frame_meshes(Some(scene), projection, origin, &vessels);
            assert_eq!(
                layers.ships.types[ShipType::Unknown.index()].triangle_count(),
                PICTURE_VESSELS * ROUND_SEGMENTS,
                "the full picture is in frame, so the worst case is the one counted",
            );
            let triangles = LayerTriangles::of(
                &layers,
                &ring,
                model::road_filter(projection.metres_per_pixel(), projection.zoom),
            );
            let latitude = model::latitude_of(center[1]);
            let longitude = center[0].to_degrees();
            assert!(
                triangles.total() <= FRAME_TRIANGLE_BUDGET,
                "{pane} pane at {zoom}× over {latitude:.3}N {longitude:.3}E builds {}, over \
                 the {FRAME_TRIANGLE_BUDGET} budget",
                triangles.describe(),
            );
            SweptFrame {
                pane,
                viewport,
                center,
                zoom,
                triangles,
            }
        };

        let mut frames: Vec<SweptFrame> = Vec::new();
        for (pane, viewport) in viewports {
            // The zoom at which a projection of this viewport reaches a metres-per-pixel
            // threshold. The switches are scale-based, so their zooms differ per pane.
            let fitted_scale = Projection::new(Camera::default(), viewport).metres_per_pixel();
            let zoom_at = |metres_per_pixel: f32| {
                (fitted_scale / metres_per_pixel).clamp(model::MIN_ZOOM, MAX_ZOOM)
            };
            let mut zooms = Vec::with_capacity(48);
            let mut zoom = model::MIN_ZOOM;
            while zoom <= MAX_ZOOM {
                zooms.push(zoom);
                zoom += 0.1;
            }
            for threshold in [
                model::ROAD_COARSE_METRES_PER_PIXEL,
                model::ROAD_MIDDLE_METRES_PER_PIXEL,
                model::ROAD_SHORT_CHAIN_METRES_PER_PIXEL,
                model::LAND_TIER_FINE_METRES_PER_PIXEL,
                model::LAND_TIER_MIDDLE_METRES_PER_PIXEL,
                model::LAND_TIER_COARSE_METRES_PER_PIXEL,
            ] {
                let switch = zoom_at(threshold);
                zooms.push(switch);
                zooms.push((switch + 0.01).min(MAX_ZOOM));
            }
            // The primary layer is gated by the zoom, not the scale.
            zooms.push(model::ROAD_PRIMARY_ZOOM);
            zooms.push((model::ROAD_PRIMARY_ZOOM + 0.01).min(MAX_ZOOM));
            zooms.sort_by(f32::total_cmp);
            zooms.dedup();

            for zoom in zooms {
                let range = centre_range(viewport, zoom);
                for (center, _place) in sample_centres(range, SWEEP_LATITUDES, SWEEP_LONGITUDES) {
                    frames.push(measure(pane, viewport, zoom, center));
                }
            }
        }
        frames.sort_by_key(|frame| std::cmp::Reverse(frame.triangles.total()));
        let mut worst = frames[0];

        // Refine around the heaviest coarse frames, two rounds each: the first samples a
        // window one coarse step either way, the second one quarter of that, so a peak
        // between two coarse samples is found to about a thirty-second of a cell.
        for seed in frames.iter().take(SWEEP_REFINEMENTS) {
            let range = centre_range(seed.viewport, seed.zoom);
            let mut step = [
                (range[1] - range[0]) / (SWEEP_LONGITUDES.max(2) - 1) as f32,
                (range[3] - range[2]) / (SWEEP_LATITUDES.max(2) - 1) as f32,
            ];
            let mut local = *seed;
            for _ in 0..SWEEP_REFINEMENT_ROUNDS {
                let window = [
                    (local.center[0] - step[0]).max(range[0]),
                    (local.center[0] + step[0]).min(range[1]),
                    (local.center[1] - step[1]).max(range[2]),
                    (local.center[1] + step[1]).min(range[3]),
                ];
                for (center, _place) in sample_centres(window, SWEEP_REFINEMENT, SWEEP_REFINEMENT) {
                    let frame = measure(seed.pane, seed.viewport, seed.zoom, center);
                    if frame.triangles.total() > local.triangles.total() {
                        local = frame;
                    }
                    if frame.triangles.total() > worst.triangles.total() {
                        worst = frame;
                    }
                }
                step = [step[0] / 4.0, step[1] / 4.0];
            }
        }

        let latitude = model::latitude_of(worst.center[1]);
        let longitude = worst.center[0].to_degrees();
        println!(
            "heaviest dynamic frame ({} pane at {}×, {latitude:.3}N {longitude:.3}E): {}",
            worst.pane,
            worst.zoom,
            worst.triangles.describe(),
        );
        println!(
            "the plan's worst-frame target was {WORST_FRAME_TARGET_TRIANGLES} triangles; the \
             dynamic half of the frame peaks at {}",
            worst.triangles.total(),
        );
        assert!(
            worst.triangles.total() <= WORST_FRAME_TRIANGLES,
            "the heaviest frame's dynamic layers build {}, over the {WORST_FRAME_TRIANGLES} \
             the whole frame used to draw at its worst",
            worst.triangles.describe(),
        );
        // A frame still draws the coast, from the static mesh: the budget has to cover that
        // too, and the widest bound on it is a whole land tier.
        let heaviest_land = scene
            .land
            .iter()
            .map(|layer| layer.triangles.len() + layer.wide.len())
            .max()
            .expect("the asset has four land tiers");
        assert!(
            worst.triangles.total() + heaviest_land <= FRAME_TRIANGLE_BUDGET,
            "one frame builds {}, and the heaviest coast can add {heaviest_land} triangles \
             from the static mesh — over the {FRAME_TRIANGLE_BUDGET} budget",
            worst.triangles.describe(),
        );
    }

    /// The pane the design measures on: the user's 1.56:1 viewport.
    const DESIGN_VIEWPORT: [f32; 2] = [1_404.0, 900.0];

    /// The coastline a frame draws follows the projection's real scale, and the ranges it
    /// selects are the chosen tier's.
    ///
    /// The land is a static mesh per tier now, so the frame does not emit its triangles:
    /// what it chooses is the tier and the ranges. The tier is asserted from the
    /// projection's real scale, the mesh's triangle count is asserted to be that tier's
    /// whole ([`LAND_TIER_TRIANGLES`]), and the selected ranges against every tier's own
    /// visible buckets — the fitted zoom drawing the coarsest tier and four times the
    /// finest, whose counts differ by a factor of nearly four.
    #[test]
    fn the_fitted_zoom_fills_the_coarsest_coast_and_four_times_fills_the_finest() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let viewport = DESIGN_VIEWPORT;
        let projection = |zoom| {
            Projection::new(
                Camera {
                    center: Camera::default().center,
                    zoom,
                }
                .clamped(viewport),
                viewport,
            )
        };

        for (zoom, expected) in [
            (model::MIN_ZOOM, model::LAND_TIERS - 1),
            (model::MAX_ZOOM, 0),
        ] {
            let projection = projection(zoom);
            assert_eq!(
                model::land_tier(projection.metres_per_pixel()),
                expected,
                "at {zoom}× the projection draws {:.0} m/px, which is not tier {expected}",
                projection.metres_per_pixel(),
            );

            let visible = projection.visible_mercator();
            let cells = model::visible_cells(visible).grown();
            let mut drawn_by_tier = Vec::with_capacity(model::LAND_TIERS);
            for tier in 0..model::LAND_TIERS {
                let layer = &scene.land[tier];
                let land = land_mesh(layer);
                let ranges = visible_land_ranges(&land, layer, cells, visible);
                drawn_by_tier.push(
                    ranges
                        .iter()
                        .map(|range| (range.end - range.start) as usize / 3)
                        .sum::<usize>(),
                );
            }

            let layer = scene.land(projection.metres_per_pixel());
            let land = land_mesh(layer);
            assert_eq!(
                triangles(&land.mesh),
                LAND_TIER_TRIANGLES[expected],
                "at {zoom}× the frame draws the triangles of tier {expected}, not another tier's",
            );
            assert_eq!(
                drawn_by_tier[expected],
                expected_land_triangles(layer, &land, cells, visible),
                "at {zoom}× the selected ranges are the visible buckets of tier {expected}",
            );
            assert!(
                drawn_by_tier[expected] > 0
                    && drawn_by_tier[expected] <= LAND_TIER_TRIANGLES[expected],
                "at {zoom}× the selection is a part of the tier: {}",
                drawn_by_tier[expected],
            );
            for (other, drawn) in drawn_by_tier.iter().enumerate() {
                if other != expected {
                    assert_ne!(
                        *drawn, drawn_by_tier[expected],
                        "tier {other} draws the same count as tier {expected} at {zoom}×, so \
                         the tier choice proves nothing: {drawn_by_tier:?}",
                    );
                }
            }
        }
    }

    /// The frame's dynamic half is inside the plan's triangle and vertex budgets, both
    /// pictures.
    ///
    /// The plan's §8 target (**≤ 21,000 triangles / ≤ 65,536 vertices**) was about the whole
    /// frame. The fitted frame's coast was 5,733 of those triangles and is drawn from the
    /// static mesh now instead of being built per frame, so the pins below are the
    /// historical totals less that coast: what a frame actually builds. Both pictures are
    /// pinned exactly as change detectors and asserted against their targets.
    #[test]
    fn the_fitted_frame_is_inside_the_plan_budget() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let viewport = DESIGN_VIEWPORT;
        let projection = Projection::new(Camera::default().clamped(viewport), viewport);
        let no_ring = Mesh::default();
        let filter = model::road_filter(projection.metres_per_pixel(), projection.zoom);

        let empty_layers = frame_meshes(Some(scene), projection, [0.0, 0.0], &[]);
        let empty = LayerTriangles::of(&empty_layers, &no_ring, filter);
        assert_eq!(
            empty.total(),
            FITTED_FRAME_TRIANGLES - FITTED_FRAME_LAND_TRIANGLES,
            "the empty fitted frame's dynamic triangle count moved: measure it, update the \
             constant and the README; {}",
            empty.describe(),
        );
        assert!(
            empty.total() <= FITTED_FRAME_TARGET_TRIANGLES,
            "the empty fitted frame builds {}, over the plan's \
             {FITTED_FRAME_TARGET_TRIANGLES} triangles",
            empty.describe(),
        );
        assert!(
            empty.total() * 3 <= FITTED_FRAME_TARGET_VERTICES,
            "the empty fitted frame emits {} vertices, over the plan's \
             {FITTED_FRAME_TARGET_VERTICES} (2^16)",
            empty.total() * 3,
        );

        let typical_layers = frame_meshes(
            Some(scene),
            projection,
            [0.0, 0.0],
            &a_typical_picture(projection),
        );
        let typical = LayerTriangles::of(&typical_layers, &no_ring, filter);
        assert_eq!(
            typical.total(),
            FITTED_FRAME_TYPICAL_TRIANGLES - FITTED_FRAME_LAND_TRIANGLES,
            "the typical fitted frame's dynamic triangle count moved: measure it, update the \
             constant and the README; {}",
            typical.describe(),
        );
        assert!(
            typical.total() <= FITTED_FRAME_TYPICAL_TARGET_TRIANGLES,
            "the typical fitted frame builds {}, over the 2^17 step's \
             {FITTED_FRAME_TYPICAL_TARGET_TRIANGLES} triangles",
            typical.describe(),
        );
        assert!(
            typical.total() * 3 <= FITTED_FRAME_TYPICAL_TARGET_TRIANGLES * 3,
            "the typical fitted frame emits {} vertices, over the 2^17 step's \
             {}",
            typical.total() * 3,
            FITTED_FRAME_TYPICAL_TARGET_TRIANGLES * 3,
        );
    }

    /// The road detail follows the picture's scale, and the three copies of class 0 never
    /// overlap: the fitted zoom draws the coarse copy, a closer one the middle copy and a
    /// closer one still the fine copy, so a road is drawn once and only once.
    #[test]
    fn zooming_in_walks_the_road_copies_from_coarse_to_fine() {
        let at_zoom = |zoom: f32, viewport: [f32; 2]| {
            Projection::new(
                Camera {
                    zoom,
                    ..Camera::default()
                },
                viewport,
            )
            .metres_per_pixel()
        };
        let design_1x = at_zoom(model::MIN_ZOOM, DESIGN_VIEWPORT);
        let design_1_6 = at_zoom(1.6, DESIGN_VIEWPORT);
        let design_2_8 = at_zoom(2.8, DESIGN_VIEWPORT);
        let wide_2048_1x = at_zoom(model::MIN_ZOOM, [2_048.0, 900.0]);

        // The class-0 copies are chosen by the picture's scale: on the design pane 1× is
        // about 1,004 m/px, so 1.6× (about 627 m/px) draws the middle copy and 2.8×
        // (about 359 m/px) the fine copy. A 2,048 px wide pane is 688 m/px at 1×, below
        // the 742 m/px coarse threshold, so it draws the middle copy even at the fitted
        // zoom. The primary layer is chosen by the zoom instead, so it joins only at the
        // closest zoom, 4×.
        assert_eq!(model::road_filter(design_1x, 1.0), RoadFilter::CoarseCopy);
        assert_eq!(
            model::road_filter(wide_2048_1x, 1.0),
            RoadFilter::MiddleCopy
        );
        assert_eq!(model::road_filter(design_1_6, 1.6), RoadFilter::MiddleCopy);
        assert_eq!(model::road_filter(design_2_8, 2.8), RoadFilter::FineCopy);
        assert_eq!(model::road_filter(design_2_8, 3.5), RoadFilter::FineCopy);
        assert_eq!(
            model::road_filter(design_2_8, 4.0),
            RoadFilter::FineCopyAndPrimary
        );

        let scene = scene::scene().expect("the checked-in asset parses");

        // At a fine scale every class-0 record is drawn by exactly one of the three
        // copies. The middle and coarse copies hold only the chains at least the build's
        // threshold long; the fine copy holds every chain, so the coarser geometries are
        // subsets of the roads the fine one covers, not a partition of them.
        let (mut coarse, mut middle, mut fine) = (0, 0, 0);
        for piece in &scene.roads.pieces {
            if piece.flags & model::ROAD_PRIMARY != 0 {
                continue;
            }
            let copies = [
                RoadFilter::CoarseCopy,
                RoadFilter::MiddleCopy,
                RoadFilter::FineCopy,
            ]
            .map(|filter| model::road_path(filter, piece, design_2_8).is_some());
            assert_eq!(
                copies.iter().filter(|drawn| **drawn).count(),
                1,
                "a class-0 piece is drawn by exactly one of the three copies",
            );
            if copies[0] {
                coarse += 1;
            }
            if copies[1] {
                middle += 1;
            }
            if copies[2] {
                fine += 1;
            }
        }
        assert_eq!(
            (coarse, middle, fine),
            (
                CLASS_ZERO_COARSE_PIECES,
                CLASS_ZERO_MIDDLE_PIECES,
                CLASS_ZERO_FINE_PIECES
            ),
            "each of the three copies keeps its own records",
        );
        assert_eq!(
            coarse + middle + fine,
            CLASS_ZERO_PIECES,
            "every class-0 record is one of the three copies",
        );

        // The coarse and middle copies are the long chains only, and the fitted zoom
        // draws every one of them whole: no chain is half drawn.
        let drawn = |filter: RoadFilter, metres_per_pixel: f32| {
            scene
                .roads
                .pieces
                .iter()
                .filter(|piece| model::road_path(filter, piece, metres_per_pixel).is_some())
                .count()
        };
        assert_eq!(
            drawn(RoadFilter::CoarseCopy, design_1x),
            CLASS_ZERO_COARSE_PIECES,
            "the fitted scale drops no coarse record",
        );
        assert_eq!(
            drawn(RoadFilter::MiddleCopy, design_1_6),
            CLASS_ZERO_MIDDLE_PIECES,
            "1.6× drops no middle record",
        );

        // The short-chain gate is a second axis: the fine copy drops every flagged piece
        // at the fitted scale and draws all of them again at 1.6×.
        let short = scene
            .roads
            .pieces
            .iter()
            .filter(|piece| piece.flags & model::ROAD_SHORT_CHAIN != 0)
            .count();
        assert!(short > 0, "the asset carries no short-chain piece");
        let fine_drawn = |metres_per_pixel: f32| {
            scene
                .roads
                .pieces
                .iter()
                .filter(|piece| {
                    model::road_path(RoadFilter::FineCopy, piece, metres_per_pixel).is_some()
                })
                .count()
        };
        assert_eq!(
            fine_drawn(design_1_6) - fine_drawn(design_1x),
            short,
            "the fitted scale drops exactly the short chains and 1.6× restores them",
        );
        assert_eq!(
            fine_drawn(design_1_6),
            CLASS_ZERO_FINE_PIECES,
            "at 1.6× the fine copy would draw every class-0 record, so every chain is back",
        );

        // And the meshes follow the scale: 1× draws the coarse copy, 1.6× the middle one,
        // 2.8× the fine one, and only 4× adds the primary roads.
        let at = |zoom: f32| {
            frame_meshes(
                Some(scene),
                Projection::new(
                    Camera {
                        center: model::project(56.0, 26.0),
                        zoom,
                    }
                    .clamped(DESIGN_VIEWPORT),
                    DESIGN_VIEWPORT,
                ),
                [0.0, 0.0],
                &[],
            )
        };
        let fitted = at(model::MIN_ZOOM);
        let middle_zoom = at(1.6);
        let detailed = at(2.8);
        assert!(
            !fitted.roads[0].is_empty()
                && !middle_zoom.roads[0].is_empty()
                && !detailed.roads[0].is_empty(),
            "every zoom band draws a class-0 copy",
        );
        assert!(
            triangles(&fitted.roads[0]) != triangles(&middle_zoom.roads[0])
                && triangles(&middle_zoom.roads[0]) != triangles(&detailed.roads[0]),
            "the three bands draw three different geometries, not the same one three times",
        );
        assert!(
            fitted.roads[1].is_empty()
                && middle_zoom.roads[1].is_empty()
                && detailed.roads[1].is_empty(),
            "primary roads wait for the closest zoom",
        );
        assert!(
            !at(4.0).roads[1].is_empty(),
            "the closest zoom draws the primary roads in view",
        );
    }
}
