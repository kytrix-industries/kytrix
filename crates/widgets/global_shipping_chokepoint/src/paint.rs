use std::ops::Range;

use ui_core::{math::Rect, paint::color::Color};

use crate::{
    colors::MapColors,
    model::{self, CellSpan, Projection, RoadFilter, Vessel},
    scene::{LandLayer, LineLayer, Piece, Scene},
    ship::{SHIP_TYPE_COUNT, SHIP_TYPES, ShipType},
};

const MARKER_WIDTH_RATIO: f32 = 0.7;

const UNKNOWN_VESSEL_RADIUS: f32 = 2.5;

const ROUND_SEGMENTS: usize = 6;
const RING_SEGMENTS: usize = 24;

const HOVER_RING_INNER: f32 = 7.0;
const HOVER_RING_OUTER: f32 = 9.0;

#[derive(Clone, Copy)]
pub(super) struct Dash {
    on: f32,
    off: f32,
    width: f32,
}

const BORDER_DASH: Dash = Dash {
    on: 6.0,
    off: 4.0,
    width: 1.6,
};

const ROAD_WIDTH: f32 = 1.4;

const ROAD_MINOR_WIDTH: f32 = 1.2;

const MARKER_LENGTH_NEAR: f32 = 10.0;
const MARKER_LENGTH_FAR: f32 = 12.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Mesh {
    pub vertices: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub bounds: Rect,
}

impl Mesh {
    pub(super) fn is_empty(&self) -> bool {
        self.triangle_count() == 0
    }

    pub(super) fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }
}

struct MeshBuilder {
    vertices: Vec<[f32; 2]>,
    indices: Vec<u32>,
    min: [f32; 2],
    max: [f32; 2],
}

impl MeshBuilder {
    fn new() -> Self {
        Self {
            vertices: Vec::new(),
            indices: Vec::new(),
            min: [f32::INFINITY; 2],
            max: [f32::NEG_INFINITY; 2],
        }
    }

    fn with_capacity(vertices: usize, indices: usize) -> Self {
        Self {
            vertices: Vec::with_capacity(vertices),
            indices: Vec::with_capacity(indices),
            ..Self::new()
        }
    }

    fn push_triangle(&mut self, corners: [[f32; 2]; 3]) {
        for [x, y] in corners {
            self.min = [self.min[0].min(x), self.min[1].min(y)];
            self.max = [self.max[0].max(x), self.max[1].max(y)];
            self.indices.push(self.vertices.len() as u32);
            self.vertices.push([x, y]);
        }
    }

    fn push_quad(&mut self, corners: [[f32; 2]; 4]) {
        self.push_triangle([corners[0], corners[1], corners[2]]);
        self.push_triangle([corners[0], corners[2], corners[3]]);
    }

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

pub(super) struct LandMesh {
    pub mesh: Mesh,
    pub cell_indices: Box<[Range<u32>]>,
    pub wide_indices: Range<u32>,
}

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

fn push_land_triangle(mesh: &mut MeshBuilder, triangle: &[f32; 6]) {
    mesh.push_triangle([
        [triangle[0], triangle[1]],
        [triangle[2], triangle[3]],
        [triangle[4], triangle[5]],
    ]);
}

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

fn triangle_bounds(triangle: &[f32; 6]) -> [f32; 4] {
    [
        triangle[0].min(triangle[2]).min(triangle[4]),
        triangle[1].min(triangle[3]).min(triangle[5]),
        triangle[0].max(triangle[2]).max(triangle[4]),
        triangle[1].max(triangle[3]).max(triangle[5]),
    ]
}

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

fn visible_pieces(layer: &LineLayer, cells: CellSpan) -> impl Iterator<Item = &Piece> {
    cells.indices().flat_map(move |cell| {
        let range = &layer.cells[cell as usize];
        layer.pieces[range.start as usize..range.end as usize].iter()
    })
}

fn piece_points<'a>(layer: &'a LineLayer, piece: &Piece) -> &'a [[f32; 2]] {
    let start = piece.start as usize;
    &layer.points[start..start + usize::from(piece.len)]
}

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

pub(super) struct ShipPaths {
    pub types: [Mesh; SHIP_TYPE_COUNT],
}

pub(super) fn ship_meshes(
    vessels: &[Vessel],
    projection: Projection,
    origin: [f32; 2],
) -> ShipPaths {
    let mut builders: [MeshBuilder; SHIP_TYPE_COUNT] = std::array::from_fn(|_| MeshBuilder::new());
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

fn canvas_point(origin: [f32; 2], canvas: [f32; 2]) -> [f32; 2] {
    [origin[0] + canvas[0], origin[1] + canvas[1]]
}

pub(super) struct MapLayers {
    pub borders: Mesh,
    pub roads: [Mesh; 2],
    pub ships: ShipPaths,
}

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

fn push_layer(layers: &mut Vec<(Mesh, Color)>, mesh: Mesh, color: Color) {
    if !mesh.is_empty() {
        layers.push((mesh, color));
    }
}

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

    fn assert_close(left: f32, right: f32, what: &str) {
        assert!(
            (left - right).abs() < 1e-4,
            "{what}: {left} should be {right}",
        );
    }

    fn triangles(mesh: &Mesh) -> usize {
        assert_eq!(
            mesh.vertices.len(),
            mesh.indices.len(),
            "a mesh has one vertex per index",
        );
        assert_eq!(mesh.indices.len() % 3, 0, "every mesh is a triangle list");
        mesh.triangle_count()
    }

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

    #[test]
    fn a_road_segment_becomes_two_triangles_at_its_configured_width() {
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

    #[test]
    fn a_border_segment_is_dashes_of_six_pixels_on_and_four_off() {
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let start = [100.0, 50.0];
        let end = [139.0, 50.0];
        let points = [projection.mercator(start), projection.mercator(end)];
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

    const LAND_TIER_TRIANGLES: [usize; model::LAND_TIERS] = [22_876, 13_347, 9_736, 5_744];

    fn land_triangle_bounds(triangle: &[f32; 6]) -> [f32; 4] {
        [
            triangle[0].min(triangle[2]).min(triangle[4]),
            triangle[1].min(triangle[3]).min(triangle[5]),
            triangle[0].max(triangle[2]).max(triangle[4]),
            triangle[1].max(triangle[3]).max(triangle[5]),
        ]
    }

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

    fn wide_reaches(layer: &LandLayer, visible: [f32; 4]) -> bool {
        layer.wide.iter().any(|triangle| {
            let [west, south, east, north] = land_triangle_bounds(triangle);
            west <= visible[2] && east >= visible[0] && south <= visible[3] && north >= visible[1]
        })
    }

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

        let off_map = model::visible_cells([-10.0, -10.0, -9.0, -9.0]);
        assert!(
            visible_land_ranges(&land, layer, off_map.grown(), [-10.0, -10.0, -9.0, -9.0])
                .is_empty(),
            "a viewport off the map draws no land",
        );
    }

    const FRAME_TRIANGLE_BUDGET: usize = 75_000;

    const CLASS_ZERO_COARSE_PIECES: usize = 4_288;

    const CLASS_ZERO_MIDDLE_PIECES: usize = 4_389;

    const CLASS_ZERO_FINE_PIECES: usize = 7_728;

    const CLASS_ZERO_PIECES: usize =
        CLASS_ZERO_COARSE_PIECES + CLASS_ZERO_MIDDLE_PIECES + CLASS_ZERO_FINE_PIECES;

    const FITTED_FRAME_TARGET_TRIANGLES: usize = 21_000;
    const FITTED_FRAME_TARGET_VERTICES: usize = 65_536;

    const WORST_FRAME_TARGET_TRIANGLES: usize = 27_000;

    const WORST_FRAME_TRIANGLES: usize = 40_631;

    const FITTED_FRAME_TRIANGLES: usize = 20_659;

    const FITTED_FRAME_LAND_TRIANGLES: usize = 5_733;

    const FITTED_FRAME_TYPICAL_TRIANGLES: usize = 23_175;

    const FITTED_FRAME_TYPICAL_TARGET_TRIANGLES: usize = 131_072 / 3;

    const REFERENCE_FRAME_2_8_TRIANGLES: usize = 31_790;

    const REFERENCE_FRAME_2_8_LAND_TRIANGLES: usize = 7_122;

    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct LayerTriangles {
        borders: usize,
        roads_coarse: usize,
        roads_middle: usize,
        roads_fine: usize,
        roads_primary: usize,
        ships: usize,
        ring: usize,
    }

    impl LayerTriangles {
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

        fn total(self) -> usize {
            self.borders
                + self.roads_coarse
                + self.roads_middle
                + self.roads_fine
                + self.roads_primary
                + self.ships
                + self.ring
        }

        fn roads(self) -> usize {
            self.roads_coarse + self.roads_middle + self.roads_fine + self.roads_primary
        }

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

    const PICTURE_VESSELS: usize = 2_516;
    const PICTURE_COLUMNS: usize = 64;
    const PICTURE_ROWS: usize = 40;

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

    const SWEEP_LATITUDES: usize = 7;
    const SWEEP_LONGITUDES: usize = 5;
    const SWEEP_REFINEMENTS: usize = 20;
    const SWEEP_REFINEMENT: usize = 9;
    const SWEEP_REFINEMENT_ROUNDS: usize = 2;

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

    #[derive(Clone, Copy, Debug)]
    struct SweptFrame {
        pane: &'static str,
        viewport: [f32; 2],
        center: [f32; 2],
        zoom: f32,
        triangles: LayerTriangles,
    }

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

    const DESIGN_VIEWPORT: [f32; 2] = [1_404.0, 900.0];

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
