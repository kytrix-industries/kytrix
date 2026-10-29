//! How one frame of the map is drawn: the ten paths a canvas paints, in the order they
//! stack, and the geometry each is built from.
//!
//! The base map is prepared once by [`crate::scene`] and the picture arrives as a list of
//! vessels, so a frame here only applies the camera's affine transform: the coastline is a
//! triangle list of the one resolution the scale justifies, the borders and the roads are
//! ribbons whose width is in screen pixels, and the borders and the lanes are dashes walked
//! along their own polylines. Nothing is tessellated per frame and nothing is cached: a
//! camera change is a different set of vertices, not a different picture.
//!
//! [`MapColors`] stays in the view, because the labels, the hover card and the hit test
//! read the same palette; this module is what turns the model's [`Projection`] into the
//! paths the canvas paints.

use gpui_kit::{Bounds, Hsla, Path, Pixels, Point, Window, point, px};

use crate::{
    model::{self, Camera, CellSpan, Projection, RoadFilter, TSS_LANES, Vessel},
    scene::{LineLayer, Piece, Scene},
    view::{MapColors, class_index, viewport_of},
};

/// How wide a marker is against its length.
const MARKER_WIDTH_RATIO: f32 = 0.7;

/// The radius of the dot a vessel whose ship record has not arrived is drawn as.
const UNKNOWN_VESSEL_RADIUS: f32 = 2.5;

/// How many segments the dot and the hover ring are drawn with.
const ROUND_SEGMENTS: usize = 8;
/// How many segments the hover ring is drawn with.
const RING_SEGMENTS: usize = 24;

/// The inner and outer radius of the ring around the vessel under the pointer.
const HOVER_RING_INNER: f32 = 7.0;
/// The outer radius of the ring around the vessel under the pointer.
const HOVER_RING_OUTER: f32 = 9.0;

/// One dashed stroke: a dash, the gap after it and the stroke's width, all in pixels.
///
/// The pattern is walked along a polyline rather than handed to a path builder: a
/// builder's `dash_array` is sampled when the path is built, which would either tessellate
/// every frame or make a 6 px dash scale with the camera. The walker's two users are the
/// lane sketch and the land borders, each with its own pattern.
#[derive(Clone, Copy)]
struct Dash {
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
    on: 5.0,
    off: 3.0,
    width: 1.6,
};

/// The dash pattern a TSS lane is drawn with.
const LANE_DASH: Dash = Dash {
    on: 6.0,
    off: 6.0,
    width: 1.5,
};

/// How wide a motorway or trunk is drawn, in pixels.
const ROAD_WIDTH: f32 = 1.4;

/// How wide a primary road is drawn, in pixels.
const ROAD_MINOR_WIDTH: f32 = 1.2;

/// The marker length at the fitted zoom, and at the closest one, in pixels.
const MARKER_LENGTH_NEAR: f32 = 10.0;
/// The marker length at [`MAX_ZOOM`].
const MARKER_LENGTH_FAR: f32 = 12.0;

/// Draws one frame of the map: the ten paths of [`MapLayers`] in their stacking order,
/// then the ring around the hovered vessel.
///
/// The geometry is [`map_layers`]'s; this function only hands the paths to the frame in
/// the order they stack.
pub(super) fn paint_map(
    bounds: Bounds<Pixels>,
    camera: Camera,
    scene: Option<&'static Scene>,
    vessels: &[Vessel],
    hovered: Option<u32>,
    colors: MapColors,
    window: &mut Window,
) {
    let projection = Projection::new(camera, viewport_of(bounds));
    if !projection.drawable() {
        return;
    }
    // A canvas paints in window coordinates, while the projection is relative to the canvas
    // box: every vertex is brought together with the box's corner here. The labels, the card
    // and the hit test deliberately stay in canvas coordinates, because they are laid out by
    // the element tree around the same box.
    let origin = [f32::from(bounds.origin.x), f32::from(bounds.origin.y)];

    for (path, color) in map_layers(scene, projection, origin, vessels).in_paint_order(colors) {
        paint_path(window, path, color);
    }

    if let Some(mmsi) = hovered
        && let Some(vessel) = vessels.iter().find(|vessel| vessel.mmsi == mmsi)
    {
        paint_path(
            window,
            ring_path(projection.screen(vessel.position), origin),
            colors.hover,
        );
    }
}

/// The ten paths one frame draws, in the order it paints them.
///
/// Building them is the frame's only geometry work, and it happens in [`map_layers`]
/// rather than in `paint_map` so the picture can be checked without a window.
struct MapLayers {
    /// The coastline, every visible triangle.
    land: Path<Pixels>,
    /// The land borders, one quad per visible dash.
    borders: Path<Pixels>,
    /// Motorways and trunks, then primary roads.
    roads: [Path<Pixels>; 2],
    /// The TSS lanes, as dashes.
    lanes: Path<Pixels>,
    /// The vessels: the unknown dots and one path per class.
    ships: ShipPaths,
}

impl MapLayers {
    /// The ten paths in the order a frame paints them, each with the colour it is drawn in.
    ///
    /// The stacking is the map's: the coastline first, the borders over it, the roads over
    /// those, the lane sketch over the roads, and the vessels last, so no part of the base
    /// map covers a vessel. The unknown dots go down before the class markers: a vessel
    /// whose ship record has arrived is a report that says more about it, and its marker
    /// should not be hidden under a neighbour's dot.
    fn in_paint_order(self, colors: MapColors) -> [(Path<Pixels>, Hsla); 10] {
        let [motorway, primary] = self.roads;
        let [tanker, container, bulk, other] = self.ships.classes;
        [
            (self.land, colors.land),
            (self.borders, colors.map_border),
            (motorway, colors.road),
            (primary, colors.road_minor),
            (self.lanes, colors.lane),
            (self.ships.unknown, colors.unknown),
            (tanker, colors.ships[0]),
            (container, colors.ships[1]),
            (bulk, colors.ships[2]),
            (other, colors.ships[3]),
        ]
    }
}

/// Builds the ten paths one frame draws, from the base map and the picture.
///
/// Pure: no window and no theme. The camera's clamped zoom decides the road detail, the
/// scale the projection draws at decides which coastline tier is filled, and the
/// viewport decides which cells are read, so two frames of the same camera and the
/// same picture build the same geometry. The land's span is grown by one cell because it
/// is bucketed by triangle centroid; the line layers were cut per cell by the build, so
/// they take the viewport's span ungrown — growing it would only revisit pieces the
/// neighbouring cells already drew.
fn map_layers(
    scene: Option<&Scene>,
    projection: Projection,
    origin: [f32; 2],
    vessels: &[Vessel],
) -> MapLayers {
    let visible = projection.visible_mercator();
    let cells = model::visible_cells(visible);
    let empty = || Path::new(point(px(0.0), px(0.0)));
    let (land, borders, roads) = match scene {
        Some(scene) => (
            land_path(scene, projection, origin, cells.grown(), visible),
            border_path(&scene.borders, projection, origin, cells, BORDER_DASH),
            road_paths(
                &scene.roads,
                projection,
                origin,
                cells,
                model::road_filter(projection.zoom),
            ),
        ),
        None => (empty(), empty(), [empty(), empty()]),
    };
    MapLayers {
        land,
        borders,
        roads,
        lanes: lane_path(projection, origin),
        ships: ship_paths(vessels, projection, origin),
    }
}

/// Hands one path to the frame, skipping the ones that hold no triangle.
fn paint_path(window: &mut Window, path: Path<Pixels>, color: Hsla) {
    if path.vertices.is_empty() {
        return;
    }
    window.paint_path(path, color);
}

/// The coastline as one fill, every visible triangle transformed into the frame's camera.
///
/// The asset packs three resolutions of the same coast; the frame draws the one whose
/// error is about a pixel at the scale it is drawing at ([`model::land_tier`]), which
/// at the fitted zoom on a typical pane is the 1 km tier rather than the 111 m one.
/// The triangles of the chosen tier were tessellated once, when the asset was parsed,
/// and bucketed into the culling grid: this walks the cells the viewport touches and
/// applies an affine transform per vertex. That is the land's only per-frame cost, and
/// it is why a cell the viewport does not touch costs nothing. `cells` is the
/// viewport's span grown by the one-cell margin; `visible` is the viewport itself,
/// which the layer needs for the triangles too wide to bucket by centroid.
fn land_path(
    scene: &Scene,
    projection: Projection,
    origin: [f32; 2],
    cells: CellSpan,
    visible: [f32; 4],
) -> Path<Pixels> {
    let mut path = Path::new(point(px(0.0), px(0.0)));
    let texture = point(0.0, 1.0);
    for triangle in scene
        .land(projection.metres_per_pixel())
        .visible(cells, visible)
    {
        let [ax, ay, bx, by, cx, cy] = *triangle;
        path.push_triangle(
            (
                canvas_point(origin, projection.screen([ax, ay])),
                canvas_point(origin, projection.screen([bx, by])),
                canvas_point(origin, projection.screen([cx, cy])),
            ),
            (texture, texture, texture),
        );
    }
    path
}

/// The land borders as one path of dashes, walked along every visible piece.
///
/// A border is furniture rather than a feature of the coastline, so its width and its dash
/// stay in pixels and do not grow with the camera. Each piece anchors the pattern at its
/// own first point: the build cut the pieces per cell, so a border crossing a cell edge
/// starts a fresh dash there rather than having its phase crawl when the camera pans.
fn border_path(
    layer: &LineLayer,
    projection: Projection,
    origin: [f32; 2],
    cells: CellSpan,
    dash: Dash,
) -> Path<Pixels> {
    let mut path = Path::new(point(px(0.0), px(0.0)));
    let texture = point(0.0, 1.0);
    for piece in visible_pieces(layer, cells) {
        let mut previous: Option<[f32; 2]> = None;
        let mut along = 0.0f32;
        for point in piece_points(layer, piece) {
            let next = projection.screen(*point);
            if let Some(start) = previous {
                let length = ((next[0] - start[0]).powi(2) + (next[1] - start[1]).powi(2)).sqrt();
                if length > 0.0 {
                    push_dashes(&mut path, start, next, along, length, dash, origin, texture);
                }
                along += length;
            }
            previous = Some(next);
        }
    }
    path
}

/// The roads of one frame, split into the two widths and colours they are drawn in.
///
/// One pass over the visible pieces: each piece is filtered and its path chosen by the
/// same [`model::road_path`] answer, so a piece cannot be drawn in a path its filter had
/// dropped. Path 0 is motorways and trunks, path 1 primary roads; at the fitted zoom the
/// second is empty, and [`paint_path`] skips it.
fn road_paths(
    layer: &LineLayer,
    projection: Projection,
    origin: [f32; 2],
    cells: CellSpan,
    filter: RoadFilter,
) -> [Path<Pixels>; 2] {
    let mut paths = [
        Path::new(point(px(0.0), px(0.0))),
        Path::new(point(px(0.0), px(0.0))),
    ];
    let widths = [ROAD_WIDTH, ROAD_MINOR_WIDTH];
    let texture = point(0.0, 1.0);
    for piece in visible_pieces(layer, cells) {
        let Some(index) = model::road_path(filter, piece) else {
            continue;
        };
        for segment in piece_points(layer, piece).windows(2) {
            push_quad(
                &mut paths[index],
                projection.screen(segment[0]),
                projection.screen(segment[1]),
                widths[index],
                origin,
                texture,
            );
        }
    }
    paths
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

/// Appends one screen-space segment as a quad `width` pixels wide: two triangles.
///
/// A segment is drawn as a ribbon rather than as a stroked path, so its width is in
/// pixels and does not grow with the camera. A segment with no length — the build can
/// store two points on top of each other — draws nothing rather than a degenerate
/// triangle.
fn push_quad(
    path: &mut Path<Pixels>,
    start: [f32; 2],
    end: [f32; 2],
    width: f32,
    origin: [f32; 2],
    texture: Point<f32>,
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
    let start_left = canvas_point(origin, side(start, 1.0));
    let start_right = canvas_point(origin, side(start, -1.0));
    let end_left = canvas_point(origin, side(end, 1.0));
    let end_right = canvas_point(origin, side(end, -1.0));
    path.push_triangle(
        (start_left, end_left, end_right),
        (texture, texture, texture),
    );
    path.push_triangle(
        (start_left, end_right, start_right),
        (texture, texture, texture),
    );
}

/// The lanes as dashes in screen space.
///
/// The dash pattern is walked along each lane rather than handed to a path builder: a
/// builder's `dash_array` is sampled when the path is built, which would either tessellate
/// every frame or make a 6 px dash scale with the camera. A dash's phase is anchored at the
/// lane's first vertex, so panning moves the dashes with the lane instead of crawling them.
fn lane_path(projection: Projection, origin: [f32; 2]) -> Path<Pixels> {
    let mut path = Path::new(point(px(0.0), px(0.0)));
    let texture = point(0.0, 1.0);
    for lane in TSS_LANES {
        let mut previous: Option<[f32; 2]> = None;
        let mut along = 0.0f32;
        for &(longitude, latitude) in lane.iter() {
            let next = projection.screen(model::project(longitude, latitude));
            if let Some(start) = previous {
                let length = ((next[0] - start[0]).powi(2) + (next[1] - start[1]).powi(2)).sqrt();
                // A lane is a long line: at four times the fitted zoom most of it is off
                // screen, and walking the dashes of a segment nothing draws is the work
                // this skips.
                if length > 0.0 && projection.segment_near(start, next, LANE_DASH.width) {
                    push_dashes(
                        &mut path, start, next, along, length, LANE_DASH, origin, texture,
                    );
                }
                along += length;
            }
            previous = Some(next);
        }
    }
    path
}

/// Appends the dashes of one screen-space segment of a dashed polyline.
fn push_dashes(
    path: &mut Path<Pixels>,
    start: [f32; 2],
    end: [f32; 2],
    along: f32,
    length: f32,
    dash: Dash,
    origin: [f32; 2],
    texture: Point<f32>,
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
        push_quad(path, at(from), at(to), dash.width, origin, texture);
    }
}

/// The vessels of one frame, one path per class plus one for the vessels without a static
/// part.
struct ShipPaths {
    /// One path per class in [`CLASSES`] order.
    classes: [Path<Pixels>; 4],
    /// The dots of the vessels a ship record has not arrived for.
    unknown: Path<Pixels>,
}

/// Splits the painted vessels into the paths one frame draws.
///
/// A vessel whose ship record has not arrived is a grey dot rather than a coloured marker:
/// the bucket is a fact about its type, and the report has not said one yet. Its position is
/// still a position, so it is drawn — a map that hid it would be reporting fewer vessels
/// than the service does.
fn ship_paths(vessels: &[Vessel], projection: Projection, origin: [f32; 2]) -> ShipPaths {
    let mut classes = [
        Path::new(point(px(0.0), px(0.0))),
        Path::new(point(px(0.0), px(0.0))),
        Path::new(point(px(0.0), px(0.0))),
        Path::new(point(px(0.0), px(0.0))),
    ];
    let mut unknown = Path::new(point(px(0.0), px(0.0)));
    let texture = point(0.0, 1.0);
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
        if !vessel.has_ship {
            push_disc(
                &mut unknown,
                position,
                UNKNOWN_VESSEL_RADIUS,
                ROUND_SEGMENTS,
                origin,
                texture,
            );
            continue;
        }
        let path = &mut classes[class_index(vessel.class)];
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
        path.push_triangle(
            (
                canvas_point(origin, tip),
                canvas_point(origin, left),
                canvas_point(origin, right),
            ),
            (texture, texture, texture),
        );
    }

    ShipPaths { classes, unknown }
}

/// Appends a filled circle centred on `center`, as a fan of triangles.
fn push_disc(
    path: &mut Path<Pixels>,
    center: [f32; 2],
    radius: f32,
    segments: usize,
    origin: [f32; 2],
    texture: Point<f32>,
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
        path.push_triangle((hub, a, b), (texture, texture, texture));
    }
}

/// The ring around the vessel under the pointer, as an annulus of triangles.
fn ring_path(center: [f32; 2], origin: [f32; 2]) -> Path<Pixels> {
    let mut path = Path::new(point(px(0.0), px(0.0)));
    let texture = point(0.0, 1.0);
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
        path.push_triangle(
            (inner_first, outer_first, outer_second),
            (texture, texture, texture),
        );
        path.push_triangle(
            (inner_first, outer_second, inner_second),
            (texture, texture, texture),
        );
    }
    path
}

/// One canvas point as the window point the paths are drawn at.
///
/// A canvas' paint pass draws in window coordinates while a projection is relative to the
/// canvas, so the box's corner is added to every vertex here.
fn canvas_point(origin: [f32; 2], canvas: [f32; 2]) -> Point<Pixels> {
    point(px(origin[0] + canvas[0]), px(origin[1] + canvas[1]))
}

#[cfg(test)]
mod tests {
    use base_ui::style::{palette::Appearance, theme::Theme};
    use gpui_kit::Pixels;

    use super::*;
    use crate::{
        model::{MAX_VESSELS, MAX_ZOOM, ShipClass},
        scene,
        view::CLASSES,
    };

    /// Asserts two floats are the same within the tolerance a projection needs.
    fn assert_close(left: f32, right: f32, what: &str) {
        assert!(
            (left - right).abs() < 1e-4,
            "{what}: {left} should be {right}",
        );
    }

    /// The paths a canvas paints are in window coordinates, while the projection and the
    /// labels are relative to the canvas box: a vertex that does not carry the box's corner
    /// draws the whole map at the window's corner, under labels laid out somewhere else.
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
        let cells = model::visible_cells(visible).grown();
        let land = land_path(scene, projection, origin, cells, visible);
        let [ax, ay, _, _, _, _] = *scene
            .land(projection.metres_per_pixel())
            .visible(cells, visible)
            .next()
            .expect("the land covers the map box");
        let expected = projection.screen([ax, ay]);
        assert_close(
            f32::from(land.vertices[0].xy_position.x),
            origin[0] + expected[0],
            "the land keeps the canvas box's x",
        );
        assert_close(
            f32::from(land.vertices[0].xy_position.y),
            origin[1] + expected[1],
            "the land keeps the canvas box's y",
        );

        let centre = projection.screen(model::project(56.0, 26.0));
        let vessel = Vessel {
            mmsi: 7,
            class: ShipClass::Tanker,
            position: model::project(56.0, 26.0),
            cog: 0,
            heading: 0,
            has_ship: true,
        };
        let ships = ship_paths(&[vessel], projection, origin);
        let tip = ships.classes[0].vertices[0].xy_position;
        assert_close(
            f32::from(tip.x),
            origin[0] + centre[0],
            "the marker's tip keeps the canvas box's x",
        );
        assert_close(
            f32::from(tip.y),
            origin[1] + centre[1] - MARKER_LENGTH_NEAR / 2.0,
            "the marker points north from its hull inside the canvas box",
        );

        let ring = ring_path(centre, origin);
        let first = ring.vertices[0].xy_position;
        assert_close(
            f32::from(first.x),
            origin[0] + centre[0] + HOVER_RING_INNER,
            "the ring keeps the canvas box's x",
        );
        assert_close(
            f32::from(first.y),
            origin[1] + centre[1],
            "the ring keeps the canvas box's y",
        );
    }

    /// One vessel of each class and one whose ship record has not arrived, all over the
    /// strait, so every vessel path has something in it.
    fn one_vessel_per_path() -> Vec<Vessel> {
        let position = model::project(56.3, 26.5);
        let mut vessels: Vec<Vessel> = CLASSES
            .iter()
            .enumerate()
            .map(|(index, class)| Vessel {
                mmsi: index as u32 + 1,
                class: *class,
                position,
                cog: 0,
                heading: 0,
                has_ship: true,
            })
            .collect();
        vessels.push(Vessel {
            mmsi: 5,
            class: ShipClass::Other,
            position,
            cog: 0,
            heading: 0,
            has_ship: false,
        });
        vessels
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

    /// The height a screen-space quad covers, in pixels.
    fn quad_height(path: &Path<Pixels>) -> f32 {
        let (low, high) = path
            .vertices
            .iter()
            .map(|vertex| f32::from(vertex.xy_position.y))
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), y| {
                (low.min(y), high.max(y))
            });
        high - low
    }

    /// Asserts two paths hold the same geometry, sampled at both ends and the middle.
    ///
    /// A path vertex has no equality of its own, so the positions are compared; the count
    /// and the samples are enough to pin the path a builder produces, because the builders
    /// emit in a fixed order. An empty expected path is a filter's answer rather than a
    /// fixture that draws nothing, so only the count is compared.
    fn assert_same_geometry(actual: &Path<Pixels>, expected: &Path<Pixels>, what: &str) {
        assert_eq!(
            actual.vertices.len(),
            expected.vertices.len(),
            "{what}: the same number of vertices",
        );
        if expected.vertices.is_empty() {
            return;
        }
        for index in [0, expected.vertices.len() / 2, expected.vertices.len() - 1] {
            assert_eq!(
                actual.vertices[index].xy_position, expected.vertices[index].xy_position,
                "{what}: vertex {index}",
            );
        }
    }

    /// The ten paths are the map's stacking: the coastline, the borders, the roads, the
    /// lanes, then the vessels.
    ///
    /// The order lives in one place — [`MapLayers::in_paint_order`] — so this is the test
    /// that replaces "did the frame paint them in order": each of the ten slots is checked
    /// against the builder its name claims, and the ten are checked against the colour
    /// roles in the documented order.
    #[test]
    fn the_map_layers_are_built_in_paint_order() {
        let colors = MapColors::resolve(&Theme::of(Appearance::Light));
        let scene = scene::scene().expect("the checked-in asset parses");
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let origin = [16.0, 16.0];
        let vessels = one_vessel_per_path();
        let layers = map_layers(Some(scene), projection, origin, &vessels);

        let visible = projection.visible_mercator();
        let cells = model::visible_cells(visible);
        let roads = road_paths(
            &scene.roads,
            projection,
            origin,
            cells,
            model::road_filter(projection.zoom),
        );
        let ships = ship_paths(&vessels, projection, origin);
        assert_same_geometry(
            &layers.land,
            &land_path(scene, projection, origin, cells.grown(), visible),
            "the land",
        );
        assert_same_geometry(
            &layers.borders,
            &border_path(&scene.borders, projection, origin, cells, BORDER_DASH),
            "the borders",
        );
        assert_same_geometry(&layers.roads[0], &roads[0], "the motorways");
        assert_same_geometry(&layers.roads[1], &roads[1], "the primary roads");
        assert_same_geometry(&layers.lanes, &lane_path(projection, origin), "the lanes");
        assert_same_geometry(&layers.ships.unknown, &ships.unknown, "the unknown dots");
        for index in 0..CLASSES.len() {
            assert_same_geometry(
                &layers.ships.classes[index],
                &ships.classes[index],
                "a class path",
            );
        }

        let painted = layers.in_paint_order(colors);
        assert_eq!(
            painted.iter().map(|(_, color)| *color).collect::<Vec<_>>(),
            [
                colors.land,
                colors.map_border,
                colors.road,
                colors.road_minor,
                colors.lane,
                colors.unknown,
                colors.ships[0],
                colors.ships[1],
                colors.ships[2],
                colors.ships[3],
            ],
            "the ten paths are painted land, borders, roads, lanes, ships, in that order",
        );
        assert!(
            !painted[0].0.vertices.is_empty()
                && !painted[1].0.vertices.is_empty()
                && !painted[2].0.vertices.is_empty()
                && !painted[4].0.vertices.is_empty(),
            "the base map has geometry at the fitted zoom",
        );
        assert!(
            painted[3].0.vertices.is_empty(),
            "the coarse copy draws no primary road, so the second road path is empty",
        );
        assert!(
            painted[5..]
                .iter()
                .all(|(path, _)| !path.vertices.is_empty()),
            "every vessel path has a vessel",
        );
    }

    /// A road segment is a screen-space ribbon: two triangles, as wide as it is configured,
    /// in the path its class names, and its width is in pixels rather than in map units.
    #[test]
    fn a_road_segment_becomes_two_triangles_at_its_configured_width() {
        // The two points share a latitude, so the segment is horizontal on screen and the
        // quad's height is exactly the width it is drawn at.
        let points = [model::project(55.0, 25.0), model::project(55.5, 25.0)];
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let origin = [0.0, 0.0];
        let cells = model::visible_cells(projection.visible_mercator());
        let motorway = one_piece_layer(&points, 1 << 1, 2_000);
        let coarse = one_piece_layer(&points, model::ROAD_COARSE, 0);
        let primary = one_piece_layer(&points, model::ROAD_PRIMARY | 1 << 1, 2_000);

        let roads = road_paths(&motorway, projection, origin, cells, RoadFilter::Fine);
        assert_eq!(roads[0].vertices.len(), 6, "one segment is two triangles");
        assert!(
            roads[1].vertices.is_empty(),
            "a motorway is not a primary road",
        );
        assert_close(
            quad_height(&roads[0]),
            ROAD_WIDTH,
            "the motorway quad is drawn as wide as it is configured",
        );

        // The coarse copy is the same width, and only the coarse filter draws it.
        assert!(
            road_paths(&coarse, projection, origin, cells, RoadFilter::Fine)[0]
                .vertices
                .is_empty(),
            "the fine copy's filter does not draw the coarse one",
        );
        let roads = road_paths(&coarse, projection, origin, cells, RoadFilter::Coarse);
        assert_eq!(
            roads[0].vertices.len(),
            6,
            "the coarse copy is a ribbon too"
        );
        assert_close(quad_height(&roads[0]), ROAD_WIDTH, "at the same width");

        let roads = road_paths(&primary, projection, origin, cells, RoadFilter::De);
        assert_eq!(
            roads[1].vertices.len(),
            6,
            "a primary road is two triangles too",
        );
        assert!(roads[0].vertices.is_empty(), "and it is not a motorway");
        assert_close(
            quad_height(&roads[1]),
            ROAD_MINOR_WIDTH,
            "the primary quad is drawn at its own width",
        );

        // The width is in screen pixels, not map units: the same piece at four times the
        // zoom is the same number of pixels wide, which is what a ribbon buys over a
        // stroke that would scale with the camera.
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
        let roads = road_paths(&motorway, close, origin, cells, RoadFilter::Fine);
        assert_eq!(
            roads[0].vertices.len(),
            6,
            "the zoom does not drop the piece"
        );
        assert_close(
            quad_height(&roads[0]),
            ROAD_WIDTH,
            "the ribbon keeps its width when the camera zooms in",
        );
    }

    /// One lane segment of 30 px is three dashes, each a quad of the configured width: the
    /// pattern is walked along the segment rather than scaled with the camera.
    #[test]
    fn a_lane_segment_is_dashes_of_six_pixels_on_and_off() {
        let mut path = Path::new(point(px(0.0), px(0.0)));
        let texture = point(0.0, 1.0);
        push_dashes(
            &mut path,
            [0.0, 50.0],
            [30.0, 50.0],
            0.0,
            30.0,
            LANE_DASH,
            [0.0, 0.0],
            texture,
        );

        // 6 px on and 6 px off: [0, 6], [12, 18] and [24, 30] are drawn.
        assert_eq!(path.vertices.len(), 18, "three dashes, two triangles each");
        assert_close(
            quad_height(&path),
            LANE_DASH.width,
            "a dash keeps its width",
        );
    }

    /// A border is dashed as well as violet, so that the kind of a line is readable from
    /// its pattern and not only from its hue: one 40 px segment of a border is five 5 px
    /// dashes with a 3 px gap after each, and the gap really is a gap.
    #[test]
    fn a_border_segment_is_dashes_of_five_pixels_on_and_three_off() {
        let mut path = Path::new(point(px(0.0), px(0.0)));
        let texture = point(0.0, 1.0);
        push_dashes(
            &mut path,
            [0.0, 50.0],
            [40.0, 50.0],
            0.0,
            40.0,
            BORDER_DASH,
            [0.0, 0.0],
            texture,
        );

        // 5 px on and 3 px off over 40 px: [0, 5], [8, 13], [16, 21], [24, 29] and
        // [32, 37] are drawn, and [37, 40] is the start of the last gap.
        assert_eq!(path.vertices.len(), 30, "five dashes, two triangles each");
        assert_close(
            quad_height(&path),
            BORDER_DASH.width,
            "a border dash keeps its width",
        );
        let span = path
            .vertices
            .iter()
            .map(|vertex| f32::from(vertex.xy_position.x))
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(low, high), x| {
                (low.min(x), high.max(x))
            });
        assert!(
            span.1 < 40.0,
            "the last dash ends at 37 px, {} before the segment does",
            40.0 - span.1,
        );
    }

    /// A pane whose asset failed has no base map, but the lanes and the picture are still
    /// drawn: a frame does not need the coastline to put something on screen.
    #[test]
    fn a_map_without_an_asset_still_draws_the_lanes_and_the_ships() {
        let projection = Projection::new(Camera::default(), [600.0, 400.0]);
        let vessels = one_vessel_per_path();
        let layers = map_layers(None, projection, [0.0, 0.0], &vessels);

        assert!(layers.land.vertices.is_empty(), "there is no coastline");
        assert!(layers.borders.vertices.is_empty(), "and no borders");
        assert!(
            layers.roads[0].vertices.is_empty() && layers.roads[1].vertices.is_empty(),
            "and no roads",
        );
        assert!(
            !layers.lanes.vertices.is_empty(),
            "the lanes are not the asset's",
        );
        assert!(!layers.ships.unknown.vertices.is_empty());
        assert!(
            layers
                .ships
                .classes
                .iter()
                .all(|path| !path.vertices.is_empty()),
            "every vessel path has a vessel",
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

    /// The most triangles one frame may draw and still hold 60 fps: 8.58 ms of fixed cost
    /// plus 107 ns per triangle has to stay inside the 16.65 ms a 60 Hz frame has.
    const FRAME_TRIANGLE_BUDGET: usize = 75_000;

    /// The class-0 pieces the coarse copy holds, as the checked-in asset stores them.
    ///
    /// Every one of them is drawn at the fitted zoom: the coarse filter drops nothing.
    const CLASS_ZERO_COARSE_PIECES: usize = 7_703;

    /// The class-0 pieces the fine copy holds.
    const CLASS_ZERO_FINE_PIECES: usize = 18_914;

    /// The class-0 pieces of both copies: the motorways and trunks of the two sections.
    const CLASS_ZERO_PIECES: usize = CLASS_ZERO_COARSE_PIECES + CLASS_ZERO_FINE_PIECES;

    /// The worst frame the viewport and zoom sweep found, in triangles.
    ///
    /// Pinned so an asset or palette change that makes a view heavier has to be measured
    /// again rather than absorbed silently into the budget. The busiest view is the 1.6×
    /// one rather than the fitted zoom: at 1× the coarsest coast is drawn (5,744
    /// triangles instead of 22,876) but the coarse copy still draws the whole motorway
    /// and trunk network, and at 1.6× the finer coast is back while the fine copy of the
    /// same network replaces the coarse one. The sweep measures it; this constant is
    /// that measurement.
    const WORST_FRAME_TRIANGLES: usize = 51_355;

    /// The triangles the fitted frame with no vessels draws on the design's pane.
    ///
    /// The ten paths of [`MapLayers`] with an empty picture: the coarsest coast, the
    /// coarse copy of the whole road network, the dashed borders and the lane sketch.
    /// Pinned because it is the number the plan's §8 **≤ 21,000** target is about; the
    /// target itself is not encoded as a constant until P2 can meet it — see
    /// `the_fitted_frame_is_pinned_until_p2`.
    const FITTED_FRAME_TRIANGLES: usize = 34_357;

    /// The triangles the ten paths of one frame hold.
    fn layer_triangles(layers: &MapLayers) -> usize {
        let paths = [
            &layers.land,
            &layers.borders,
            &layers.roads[0],
            &layers.roads[1],
            &layers.lanes,
            &layers.ships.unknown,
            &layers.ships.classes[0],
            &layers.ships.classes[1],
            &layers.ships.classes[2],
            &layers.ships.classes[3],
        ];
        paths
            .iter()
            .map(|path| {
                assert_eq!(path.vertices.len() % 3, 0, "every path is a triangle list",);
                path.vertices.len() / 3
            })
            .sum()
    }

    /// Every triangle one frame draws: the ten paths and the hover ring.
    ///
    /// The ring is the only geometry outside [`MapLayers`], and it is drawn only when a
    /// vessel is hovered; counting it always is the worst case. The labels, the legend,
    /// the card, the badge, the empty state and the canvas' own sea fill are elements
    /// rather than path triangles, and the plan's 8.58 ms intercept already carries them,
    /// which is why they are not counted here. The lanes and the vessels are part of the
    /// ten paths, so nothing is added twice.
    fn frame_triangles(layers: &MapLayers, ring: &Path<Pixels>) -> usize {
        assert_eq!(
            ring.vertices.len() % 3,
            0,
            "the ring is a triangle list too",
        );
        layer_triangles(layers) + ring.vertices.len() / 3
    }

    /// The service's bound on one picture, with every vessel at its most expensive.
    ///
    /// A vessel whose ship record has not arrived is a disc of [`ROUND_SEGMENTS`]
    /// triangles; a vessel with one is a single marker triangle. Spreading `MAX_VESSELS`
    /// unknown vessels over the viewport therefore counts the most the vessel layer can
    /// ever draw — no allowance, and no assumption about how many records have arrived.
    fn a_full_picture(projection: Projection) -> Vec<Vessel> {
        (0..MAX_VESSELS)
            .map(|index| {
                let column = (index % 40) as f32;
                let row = (index / 40) as f32;
                let canvas = [
                    projection.viewport[0] * (column + 0.5) / 40.0,
                    projection.viewport[1] * (row + 0.5) / 25.0,
                ];
                Vessel {
                    mmsi: index as u32 + 1,
                    class: ShipClass::Tanker,
                    position: projection.mercator(canvas),
                    cog: 0,
                    heading: 0,
                    has_ship: false,
                }
            })
            .collect()
    }

    /// The picture has to fit a 60 Hz frame in the worst view the camera can be left in.
    ///
    /// The totals are the triangles the frame really draws: every path of [`MapLayers`]
    /// plus the hover ring, with a full picture of [`MAX_VESSELS`] unknown vessels — the
    /// worst each vessel can cost. Nothing is set aside as an allowance.
    ///
    /// The fit covers the viewport, so 1× does not show the whole map. The fitted zoom is
    /// no longer the busiest view: it draws the coarsest coast, while 1.6× switches to the
    /// finer one and to the fine copy of the road network at the same time, and that is
    /// where the sweep's worst frame sits. The sweep below covers the user's 1.56:1 pane,
    /// a wide one and a tall one, at the fitted zoom and at every zoom the road detail
    /// switches at, over the map's middle, Dubai and the strait — the shots the design's
    /// budget was first measured on.
    #[test]
    fn the_visible_triangle_budget_is_under_the_frame_budget() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let origin = [0.0, 0.0];
        let ring = ring_path([100.0, 100.0], origin);

        let viewports = [
            ("1.56", [1_404.0, 900.0]),
            ("wide", [1_600.0, 866.0]),
            ("tall", [900.0, 1_400.0]),
        ];
        let zooms = [
            model::MIN_ZOOM,
            model::ROAD_LOD_ZOOM[0],
            model::ROAD_LOD_ZOOM[1],
            MAX_ZOOM,
        ];
        let centres = [
            ("the middle", Camera::default().center),
            ("Dubai", model::project(55.30, 25.27)),
            ("the strait", model::project(56.35, 26.30)),
        ];

        let mut worst = (0usize, "no view".to_string());
        for (pane, viewport) in viewports {
            for zoom in zooms {
                for (place, center) in centres {
                    let projection =
                        Projection::new(Camera { center, zoom }.clamped(viewport), viewport);
                    let layers =
                        map_layers(Some(scene), projection, origin, &a_full_picture(projection));
                    assert_eq!(
                        layers.ships.unknown.vertices.len() / 3,
                        MAX_VESSELS * ROUND_SEGMENTS,
                        "the full picture is in frame, so the worst case is the one counted",
                    );
                    let triangles = frame_triangles(&layers, &ring);
                    assert!(
                        triangles <= FRAME_TRIANGLE_BUDGET,
                        "{pane} pane at {zoom}× over {place} draws {triangles} triangles, over \
                         the {FRAME_TRIANGLE_BUDGET} budget",
                    );
                    if triangles > worst.0 {
                        worst = (triangles, format!("{pane} pane at {zoom}× over {place}"));
                    }
                }
            }
        }

        assert_eq!(
            worst.0, WORST_FRAME_TRIANGLES,
            "the sweep's worst frame is {} triangles ({})",
            worst.0, worst.1,
        );
    }

    /// The pane the design measures on: the user's 1.56:1 viewport.
    const DESIGN_VIEWPORT: [f32; 2] = [1_404.0, 900.0];

    /// The coastline a frame draws follows the projection's real scale, and the vertices
    /// it emits are exactly the chosen tier's visible triangles.
    ///
    /// This is asserted by comparing the painted vertex count with the layer's own
    /// [`LandLayer::visible`](crate::scene::LandLayer::visible) count for every tier, not
    /// by looking at whether anything was painted: the fitted zoom has to match the
    /// coarsest tier's count and four times the zoom the finest tier's, and the two
    /// counts differ by a factor of five.
    #[test]
    fn the_fitted_zoom_fills_the_coarsest_coast_and_four_times_fills_the_finest() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let viewport = DESIGN_VIEWPORT;
        let origin = [0.0, 0.0];
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
            let visible = projection.visible_mercator();
            let cells = model::visible_cells(visible).grown();
            let path = land_path(scene, projection, origin, cells, visible);
            let triangles: Vec<usize> = scene
                .land
                .iter()
                .map(|land| land.visible(cells, visible).count())
                .collect();

            assert_eq!(
                model::land_tier(projection.metres_per_pixel()),
                expected,
                "at {zoom}× the projection draws {:.0} m/px, which is not tier {expected}",
                projection.metres_per_pixel(),
            );
            assert_eq!(
                path.vertices.len(),
                triangles[expected] * 3,
                "at {zoom}× the frame paints the triangles of tier {expected}, not another \
                 tier's: {triangles:?}",
            );
            for other in 0..model::LAND_TIERS {
                if other != expected {
                    assert_ne!(
                        triangles[other], triangles[expected],
                        "tier {other} draws the same count as tier {expected}, so the \
                         assertion above proves nothing",
                    );
                }
            }
        }
    }

    /// The fitted frame's triangle count, pinned until P2 lands.
    ///
    /// This is a change detector, not a budget assertion: the count is over the plan's
    /// §8 target of 21,000 because the road layer is still at P1's 26,024 triangles,
    /// and asserting "over the target" would pass at 21,500 and fail at 20,900 — the
    /// opposite of what the target means. So the test pins the exact number, and the
    /// **P2 TODO** is to replace that pin with `assert!(triangles <= 21_000)`: the road
    /// thinning is what brings this frame from the pinned figure down under 21,000.
    ///
    /// The plan's worst-frame target of 27,000 is measured with a full 1,000-vessel
    /// picture (`the_visible_triangle_budget_is_under_the_frame_budget`); it needs P2's
    /// roads and P4's vessel dots together, and neither is part of P1.
    #[test]
    fn the_fitted_frame_is_pinned_until_p2() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let viewport = DESIGN_VIEWPORT;
        let projection = Projection::new(Camera::default().clamped(viewport), viewport);
        let layers = map_layers(Some(scene), projection, [0.0, 0.0], &[]);
        let triangles = layer_triangles(&layers);
        assert_eq!(
            triangles, FITTED_FRAME_TRIANGLES,
            "the fitted frame's triangle count moved: measure it, update the constant and \
             the README, and check it against the plan's 21,000 target",
        );
    }

    /// The road detail follows the camera, and the two copies of class 0 never overlap: the
    /// fitted zoom draws the coarse copy and a closer one the fine copy, so a road is drawn
    /// once and only once.
    #[test]
    fn zooming_in_switches_from_the_coarse_copy_to_the_fine_one() {
        assert_eq!(model::road_filter(1.5), RoadFilter::Coarse);
        assert_eq!(model::road_filter(2.0), RoadFilter::Fine);
        assert_eq!(model::road_filter(3.0), RoadFilter::De);

        let scene = scene::scene().expect("the checked-in asset parses");

        // Every class-0 piece is drawn by exactly one of the two copies, and the fitted
        // zoom draws all of the coarse one — no `ref` and no length test is left. That is
        // the whole reason the coarse copy exists: 7,703 pieces are drawn at 1× out of
        // the 26,617 class-0 pieces the asset stores, and the drawn part is a different
        // geometry from the rest rather than a subset of it.
        let (mut coarse, mut fine) = (0, 0);
        for piece in &scene.roads.pieces {
            if piece.flags & model::ROAD_PRIMARY != 0 {
                continue;
            }
            let in_coarse = model::road_path(RoadFilter::Coarse, piece).is_some();
            let in_fine = model::road_path(RoadFilter::Fine, piece).is_some();
            assert_ne!(
                in_coarse, in_fine,
                "a class-0 piece is drawn by exactly one of the two copies",
            );
            if in_coarse {
                coarse += 1;
            } else {
                fine += 1;
            }
        }
        assert_eq!(
            (coarse, fine),
            (CLASS_ZERO_COARSE_PIECES, CLASS_ZERO_FINE_PIECES)
        );
        assert_eq!(
            coarse + fine,
            CLASS_ZERO_PIECES,
            "the two copies are the whole of class 0",
        );

        // And the layers follow the camera: the fitted zoom draws no primary road, four
        // times the zoom does.
        let at = |zoom: f32| {
            map_layers(
                Some(scene),
                Projection::new(
                    Camera {
                        center: model::project(56.0, 26.0),
                        zoom,
                    },
                    [600.0, 400.0],
                ),
                [0.0, 0.0],
                &[],
            )
        };
        let fitted = at(model::MIN_ZOOM);
        assert!(
            !fitted.roads[0].vertices.is_empty(),
            "the fitted zoom draws the coarse copy",
        );
        assert!(
            fitted.roads[1].vertices.is_empty(),
            "primary roads wait for the densest filter",
        );
        let detailed = at(model::ROAD_LOD_ZOOM[0]);
        assert!(
            !detailed.roads[0].vertices.is_empty(),
            "the first threshold switches to the fine copy",
        );
        assert_ne!(
            fitted.roads[0].vertices.len(),
            detailed.roads[0].vertices.len(),
            "the two zooms draw two different geometries, not the same one twice",
        );
        assert!(
            !at(3.0).roads[1].vertices.is_empty(),
            "D+E draws the primary roads in view",
        );
    }
}
