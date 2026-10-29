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
    on: 6.0,
    off: 4.0,
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
/// Pure: no window and no theme. The scale the projection draws at decides which
/// coastline tier is filled and which class-0 road copy is drawn, the clamped zoom
/// decides when the primary roads join it, and the viewport decides which cells are
/// read, so two frames of the same camera and the same picture build the same geometry.
/// The land's span is grown by one cell because it is bucketed by triangle centroid; the
/// line layers were cut per cell by the build, so they take the viewport's span ungrown
/// — growing it would only revisit pieces the neighbouring cells already drew.
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
                model::road_filter(projection.metres_per_pixel(), projection.zoom),
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
    let metres_per_pixel = projection.metres_per_pixel();
    for piece in visible_pieces(layer, cells) {
        let Some(index) = model::road_path(filter, piece, metres_per_pixel) else {
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
            model::road_filter(projection.metres_per_pixel(), projection.zoom),
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

        let roads = road_paths(&motorway, projection, origin, cells, RoadFilter::FineCopy);
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
            road_paths(&coarse, projection, origin, cells, RoadFilter::FineCopy)[0]
                .vertices
                .is_empty(),
            "the fine copy's filter does not draw the coarse one",
        );
        let roads = road_paths(&coarse, projection, origin, cells, RoadFilter::CoarseCopy);
        assert_eq!(
            roads[0].vertices.len(),
            6,
            "the coarse copy is a ribbon too"
        );
        assert_close(quad_height(&roads[0]), ROAD_WIDTH, "at the same width");

        let roads = road_paths(
            &primary,
            projection,
            origin,
            cells,
            RoadFilter::FineCopyAndPrimary,
        );
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
        let roads = road_paths(&motorway, close, origin, cells, RoadFilter::FineCopy);
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
    /// its pattern and not only from its hue: one 40 px segment of a border is four 6 px
    /// dashes with a 4 px gap after each, and the gap really is a gap.
    #[test]
    fn a_border_segment_is_dashes_of_six_pixels_on_and_four_off() {
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

        // 6 px on and 4 px off over 40 px: [0, 6], [10, 16], [20, 26] and [30, 36]
        // are drawn, and [36, 40] is the last gap.
        assert_eq!(path.vertices.len(), 24, "four dashes, two triangles each");
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
            "the last dash ends at 36 px, {} before the segment does",
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
    /// The coarse copy is the chains at least the build's 0.3 km feature-dropping
    /// threshold long; every one of them is drawn at the fitted zoom. The short chains
    /// are absent here and come back through the fine copy at 1.6× on the design pane.
    const CLASS_ZERO_COARSE_PIECES: usize = 4_288;

    /// The class-0 pieces the fine copy holds: every chain, including the short ones the
    /// runtime gates below [`model::ROAD_SHORT_CHAIN_METRES_PER_PIXEL`].
    const CLASS_ZERO_FINE_PIECES: usize = 7_728;

    /// The class-0 records of both copies. Not a partition of the roads: the coarse copy
    /// is a second geometry of the long chains the fine copy also holds, so the two
    /// counts overlap in what they represent even though no record is in both.
    const CLASS_ZERO_PIECES: usize = CLASS_ZERO_COARSE_PIECES + CLASS_ZERO_FINE_PIECES;

    /// The plan's §8 targets for the fitted frame: triangles and vertices.
    ///
    /// Both are met by [`FITTED_FRAME_TRIANGLES`]: 20,801 triangles and 62,403 vertices
    /// against 21,000 and 65,536 (199 and 3,133 to spare). The vertex target is the
    /// 2^16 instance-buffer step the plan's §1.1 explains.
    const FITTED_FRAME_TARGET_TRIANGLES: usize = 21_000;
    const FITTED_FRAME_TARGET_VERTICES: usize = 65_536;

    /// The plan's §8 target for the worst frame, in triangles.
    ///
    /// **P2's acceptance does not include this frame**: it is moved to P4, which adds a
    /// 0.002° land tier and the six-segment vessel dots. P2 pins the measured frame and
    /// documents the gap; see [`WORST_FRAME_TRIANGLES`].
    const WORST_FRAME_TARGET_TRIANGLES: usize = 27_000;

    /// The worst frame the viewport and zoom sweep found, in triangles.
    ///
    /// Pinned so an asset or palette change that makes a view heavier has to be measured
    /// again rather than absorbed silently into the budget. The busiest view is the
    /// **2,048 px wide pane at 1.539×**, just past the land tier's 450 m/px switch: the
    /// land is the 0.001° tier there (18,126 triangles), the fine roads 11,988, the
    /// borders 974, the lanes 302, the full vessel picture 8,000 and the ring 48 —
    /// 39,438 in all, 12,438 over the plan's 27,000.
    ///
    /// The 2,048 pane is 688 m/px at 1×, below the coarse copy's 742 m/px threshold, so
    /// it draws the fine copy at every zoom; its 1× frame is 35,267. The design pane's
    /// 4× frame is 26,222 (under the target), 3.49× is 26,494, 2.8× is 30,135
    /// ([`REFERENCE_FRAME_2_8_TRIANGLES`]). The remaining gap is land tier 0 plus the
    /// vessel layer, and P4 owns it: a 0.002° land tier would cut the 18,126, and the
    /// six-segment dots turn the 8,000 into 6,000. A middle road tier could cut the
    /// 11,988 fine triangles, but the land term alone keeps the frame in 2^17, so road
    /// thinning would save CPU only, not the instance-buffer step. P2 therefore records
    /// the frame instead of closing it.
    const WORST_FRAME_TRIANGLES: usize = 39_438;

    /// The triangles the fitted frame with no vessels draws on the design's pane.
    ///
    /// The ten paths of [`MapLayers`] with an empty picture and no hover ring: the
    /// coarsest coast, the coarse copy of the long road chains, the dashed borders and
    /// the lane sketch. 5,733 + 2,312 + 12,614 + 142 = 20,801, inside both §8 targets
    /// (21,000 triangles / 65,536 vertices = 62,403).
    const FITTED_FRAME_TRIANGLES: usize = 20_801;

    /// The triangles the fitted frame draws with the typical picture: 1,000 vessels
    /// whose ship record has arrived, one marker triangle each.
    ///
    /// Pinned separately from [`FITTED_FRAME_TRIANGLES`] because the empty frame's 21,000
    /// target does not cover it: the vessel layer adds 1,000 triangles, and the frame's
    /// gate is the 2^16 instance-buffer step instead (see
    /// [`FITTED_FRAME_TYPICAL_TARGET_TRIANGLES`]). It is 21,801 triangles / 65,403
    /// vertices — 44 triangles and 133 vertices under the step, the margin the border
    /// dash widening bought.
    const FITTED_FRAME_TYPICAL_TRIANGLES: usize = 21_801;

    /// The 2^16 instance-buffer step in triangles: `65,536 / 3` rounded down.
    ///
    /// The typical fitted frame is allowed to sit in the 2^16 step — that is the memory
    /// cliff the plan's §1.1 is about — while the empty frame keeps the stricter §8
    /// 21,000 target.
    const FITTED_FRAME_TYPICAL_TARGET_TRIANGLES: usize = 65_536 / 3;

    /// The triangles the 2.8× design frame draws with a full picture.
    ///
    /// Pinned separately because it is the frame the plan's worst-case analysis names:
    /// the primary roads now wait for the closest zoom, so this frame is the fine copy
    /// only. It is 30,135 (land 12,183 + borders 784 + fine 8,740 + lanes 380 + ships
    /// 8,000 + ring 48), 3,135 over the plan's 27,000. The sweep's actual worst is the
    /// 2,048 px pane's 1.539× land-tier switch (39,438), not this frame; the pin keeps
    /// the plan's reference measured.
    const REFERENCE_FRAME_2_8_TRIANGLES: usize = 30_135;

    /// One frame's triangles, layer by layer.
    ///
    /// The budget is a sum, so when a frame goes over it the question is always *which*
    /// layer did: this keeps the road layer split into the two copies the filter chooses
    /// between and the primary roads it may add, and every assertion that fails prints
    /// the whole breakdown rather than only the total.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct LayerTriangles {
        /// The coastline tier the projection draws.
        land: usize,
        /// The dashed land borders.
        borders: usize,
        /// The coarse copy of class 0, drawn while the scale is coarse.
        roads_coarse: usize,
        /// The fine copy of class 0, drawn once the coarse copy is too coarse.
        roads_fine: usize,
        /// The primary roads, drawn only at the closest zooms.
        roads_primary: usize,
        /// The dashed TSS lanes.
        lanes: usize,
        /// The vessel layer.
        ships: usize,
        /// The hover ring, which is outside [`MapLayers`].
        ring: usize,
    }

    impl LayerTriangles {
        /// The triangles `layers` draws under `filter`, plus the hover ring.
        ///
        /// The ring is drawn only when a vessel is hovered; counting it always is the
        /// worst case. The labels, the legend, the card, the badge, the empty state and
        /// the canvas' own sea fill are elements rather than path triangles, and the
        /// plan's 8.58 ms intercept already carries them, which is why they are not
        /// counted here. The lanes and the vessels are part of the ten paths, so nothing
        /// is added twice.
        fn of(layers: &MapLayers, ring: &Path<Pixels>, filter: RoadFilter) -> Self {
            let [class_zero, primary] = &layers.roads;
            let (roads_coarse, roads_fine) = match filter {
                RoadFilter::CoarseCopy => (triangles(class_zero), 0),
                RoadFilter::FineCopy | RoadFilter::FineCopyAndPrimary => (0, triangles(class_zero)),
            };
            Self {
                land: triangles(&layers.land),
                borders: triangles(&layers.borders),
                roads_coarse,
                roads_fine,
                roads_primary: triangles(primary),
                lanes: triangles(&layers.lanes),
                ships: triangles(&layers.ships.unknown)
                    + layers.ships.classes.iter().map(triangles).sum::<usize>(),
                ring: triangles(ring),
            }
        }

        /// The triangles of all layers.
        fn total(self) -> usize {
            self.land
                + self.borders
                + self.roads_coarse
                + self.roads_fine
                + self.roads_primary
                + self.lanes
                + self.ships
                + self.ring
        }

        /// The road layer's triangles: the copy in use plus the primary roads.
        fn roads(self) -> usize {
            self.roads_coarse + self.roads_fine + self.roads_primary
        }

        /// The breakdown as one line, for an assertion that has to say where the budget
        /// went.
        fn describe(self) -> String {
            format!(
                "land {}, borders {}, roads {} (coarse {}, fine {}, primary {}), lanes {}, \
                 ships {}, ring {} = {} triangles",
                self.land,
                self.borders,
                self.roads(),
                self.roads_coarse,
                self.roads_fine,
                self.roads_primary,
                self.lanes,
                self.ships,
                self.ring,
                self.total(),
            )
        }
    }

    /// The triangles one path holds.
    fn triangles(path: &Path<Pixels>) -> usize {
        assert_eq!(path.vertices.len() % 3, 0, "every path is a triangle list");
        path.vertices.len() / 3
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

    /// The service's bound on one picture, with every vessel's ship record arrived.
    ///
    /// A vessel with a ship record is one marker triangle, which is the typical frame;
    /// [`a_full_picture`] is the worst one (an eight-segment dot per vessel).
    fn a_typical_picture(projection: Projection) -> Vec<Vessel> {
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
                    has_ship: true,
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
    /// The fit covers the viewport, so 1× does not show the whole map. The busiest view is
    /// not the fitted one: the coast and the road copy both get heavier as the camera
    /// zooms in, and the peak sits just past a switch — see [`WORST_FRAME_TRIANGLES`] for
    /// the view the current asset peaks at. The sweep below covers the user's 1.56:1 pane,
    /// 1,600 and 2,048 px wide panes and a tall one, over the map's middle, Dubai and the
    /// strait, at a dense zoom walk plus the exact zooms every switch happens at — the
    /// road copy, the short-chain gate and the two land tiers — because the count is
    /// piecewise constant between switches and peaks just above each one.
    #[test]
    fn the_visible_triangle_budget_is_under_the_frame_budget() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let origin = [0.0, 0.0];
        let ring = ring_path([100.0, 100.0], origin);
        let no_ring = Path::new(point(px(0.0), px(0.0)));
        let at = |viewport: [f32; 2], zoom: f32, center: [f32; 2]| {
            Projection::new(Camera { center, zoom }.clamped(viewport), viewport)
        };
        let breakdown = |projection: Projection, vessels: &[Vessel], ring: &Path<Pixels>| {
            let layers = map_layers(Some(scene), projection, origin, vessels);
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
            "1× fitted frame, 1,000 single-triangle vessels: {}",
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
        assert_eq!(
            reference.total(),
            REFERENCE_FRAME_2_8_TRIANGLES,
            "the 2.8× design frame moved: {}; the plan's worst-frame target is \
             {WORST_FRAME_TARGET_TRIANGLES} and this frame is {} over it",
            reference.describe(),
            reference
                .total()
                .saturating_sub(WORST_FRAME_TARGET_TRIANGLES),
        );

        let viewports = [
            ("1.56", [1_404.0, 900.0]),
            ("wide", [1_600.0, 866.0]),
            ("wide-2048", [2_048.0, 900.0]),
            ("tall", [900.0, 1_400.0]),
        ];
        let centres = [
            ("the middle", Camera::default().center),
            ("Dubai", model::project(55.30, 25.27)),
            ("the strait", model::project(56.35, 26.30)),
        ];

        let mut worst = (LayerTriangles::default(), "no view".to_string());
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
                model::ROAD_SHORT_CHAIN_METRES_PER_PIXEL,
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
                for (place, center) in centres {
                    let projection = at(viewport, zoom, center);
                    let vessels = a_full_picture(projection);
                    let layers = map_layers(Some(scene), projection, origin, &vessels);
                    assert_eq!(
                        layers.ships.unknown.vertices.len() / 3,
                        MAX_VESSELS * ROUND_SEGMENTS,
                        "the full picture is in frame, so the worst case is the one counted",
                    );
                    let triangles = LayerTriangles::of(
                        &layers,
                        &ring,
                        model::road_filter(projection.metres_per_pixel(), projection.zoom),
                    );
                    assert!(
                        triangles.total() <= FRAME_TRIANGLE_BUDGET,
                        "{pane} pane at {zoom}× over {place} draws {}, over the \
                         {FRAME_TRIANGLE_BUDGET} budget",
                        triangles.describe(),
                    );
                    if triangles.total() > worst.0.total() {
                        worst = (triangles, format!("{pane} pane at {zoom}× over {place}"));
                    }
                }
            }
        }

        println!("worst frame ({}): {}", worst.1, worst.0.describe());
        assert_eq!(
            worst.0.total(),
            WORST_FRAME_TRIANGLES,
            "the sweep's worst frame is {}; the plan's target is \
             {WORST_FRAME_TARGET_TRIANGLES}, {} over it",
            worst.0.describe(),
            worst.0.total().saturating_sub(WORST_FRAME_TARGET_TRIANGLES),
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

    /// The fitted frame is inside the plan's triangle and vertex budgets, both pictures.
    ///
    /// The empty frame is the one the plan's §8 **≤ 21,000 triangles / ≤ 65,536
    /// vertices** target is about. The typical frame has 1,000 vessels whose ship record
    /// has arrived — one marker triangle each — and its gate is the 2^16
    /// instance-buffer step: **≤ 21,845 triangles / ≤ 65,536 vertices**. Both are pinned
    /// exactly as change detectors and asserted against their targets.
    #[test]
    fn the_fitted_frame_is_inside_the_plan_budget() {
        let scene = scene::scene().expect("the checked-in asset parses");
        let viewport = DESIGN_VIEWPORT;
        let projection = Projection::new(Camera::default().clamped(viewport), viewport);
        let no_ring = Path::new(point(px(0.0), px(0.0)));
        let filter = model::road_filter(projection.metres_per_pixel(), projection.zoom);

        let empty_layers = map_layers(Some(scene), projection, [0.0, 0.0], &[]);
        let empty = LayerTriangles::of(&empty_layers, &no_ring, filter);
        assert_eq!(
            empty.total(),
            FITTED_FRAME_TRIANGLES,
            "the empty fitted frame's triangle count moved: measure it, update the \
             constant and the README; {}",
            empty.describe(),
        );
        assert!(
            empty.total() <= FITTED_FRAME_TARGET_TRIANGLES,
            "the empty fitted frame draws {}, over the plan's \
             {FITTED_FRAME_TARGET_TRIANGLES} triangles",
            empty.describe(),
        );
        assert!(
            empty.total() * 3 <= FITTED_FRAME_TARGET_VERTICES,
            "the empty fitted frame emits {} vertices, over the plan's \
             {FITTED_FRAME_TARGET_VERTICES} (2^16)",
            empty.total() * 3,
        );

        let typical_layers = map_layers(
            Some(scene),
            projection,
            [0.0, 0.0],
            &a_typical_picture(projection),
        );
        let typical = LayerTriangles::of(&typical_layers, &no_ring, filter);
        assert_eq!(
            typical.total(),
            FITTED_FRAME_TYPICAL_TRIANGLES,
            "the typical fitted frame's triangle count moved: measure it, update the \
             constant and the README; {}",
            typical.describe(),
        );
        assert!(
            typical.total() <= FITTED_FRAME_TYPICAL_TARGET_TRIANGLES,
            "the typical fitted frame draws {}, over the 2^16 step's \
             {FITTED_FRAME_TYPICAL_TARGET_TRIANGLES} triangles",
            typical.describe(),
        );
        assert!(
            typical.total() * 3 <= FITTED_FRAME_TARGET_VERTICES,
            "the typical fitted frame emits {} vertices, over the 2^16 step's \
             {FITTED_FRAME_TARGET_VERTICES}",
            typical.total() * 3,
        );
    }

    /// The road detail follows the picture's scale, and the two copies of class 0 never
    /// overlap: the fitted zoom draws the coarse copy and a closer one the fine copy, so
    /// a road is drawn once and only once.
    #[test]
    fn zooming_in_switches_from_the_coarse_copy_to_the_fine_one() {
        let design_1x = Projection::new(Camera::default(), DESIGN_VIEWPORT).metres_per_pixel();
        let design_1_6 = Projection::new(
            Camera {
                zoom: 1.6,
                ..Camera::default()
            },
            DESIGN_VIEWPORT,
        )
        .metres_per_pixel();
        let wide_2048_1x = Projection::new(Camera::default(), [2_048.0, 900.0]).metres_per_pixel();

        // The class-0 copies are chosen by the picture's scale: on the design pane 1× is
        // about 1,004 m/px, so 1.6× (about 627 m/px) draws the fine copy. A 2,048 px
        // wide pane is 688 m/px at 1×, below the 742 m/px coarse threshold, so it draws
        // the fine copy even at the fitted zoom. The primary layer is chosen by the zoom
        // instead, so it joins only at the closest zoom, 4×.
        assert_eq!(model::road_filter(design_1x, 1.0), RoadFilter::CoarseCopy);
        assert_eq!(model::road_filter(wide_2048_1x, 1.0), RoadFilter::FineCopy);
        assert_eq!(model::road_filter(design_1_6, 1.6), RoadFilter::FineCopy);
        assert_eq!(model::road_filter(design_1_6, 2.8), RoadFilter::FineCopy);
        assert_eq!(model::road_filter(design_1_6, 3.5), RoadFilter::FineCopy);
        assert_eq!(
            model::road_filter(design_1_6, 4.0),
            RoadFilter::FineCopyAndPrimary
        );

        let scene = scene::scene().expect("the checked-in asset parses");

        // Every class-0 record is drawn by exactly one of the two copies. The coarse copy
        // holds only the chains at least the build's threshold long; the fine copy holds
        // every chain, so the coarse geometry is a subset of the roads the fine one
        // covers, not a partition of them.
        let (mut coarse, mut fine) = (0, 0);
        for piece in &scene.roads.pieces {
            if piece.flags & model::ROAD_PRIMARY != 0 {
                continue;
            }
            let in_coarse = model::road_path(RoadFilter::CoarseCopy, piece, design_1_6).is_some();
            let in_fine = model::road_path(RoadFilter::FineCopy, piece, design_1_6).is_some();
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
            "every class-0 record is one of the two copies",
        );

        // The short-chain gate is a second axis: at the fitted scale the fine copy drops
        // every flagged piece, and at 1.6× it draws all of them again. The difference is
        // exactly the flagged pieces, so a chain cannot be half drawn.
        let short = scene
            .roads
            .pieces
            .iter()
            .filter(|piece| piece.flags & model::ROAD_SHORT_CHAIN != 0)
            .count();
        assert!(short > 0, "the asset carries no short-chain piece");
        let drawn = |metres_per_pixel: f32| {
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
            drawn(design_1_6) - drawn(design_1x),
            short,
            "the fitted scale drops exactly the short chains and 1.6× restores them",
        );
        assert_eq!(
            drawn(design_1_6),
            CLASS_ZERO_FINE_PIECES,
            "at 1.6× the fine copy draws every class-0 record, so every chain is back",
        );
        let coarse_at_1x = scene
            .roads
            .pieces
            .iter()
            .filter(|piece| model::road_path(RoadFilter::CoarseCopy, piece, design_1x).is_some())
            .count();
        assert_eq!(
            coarse_at_1x, CLASS_ZERO_COARSE_PIECES,
            "the fitted scale drops no coarse record: every chain it draws is whole",
        );

        // And the layers follow the scale: 1× draws no primary road, 4× does.
        let at = |zoom: f32| {
            map_layers(
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
        assert!(
            !fitted.roads[0].vertices.is_empty(),
            "the fitted zoom draws the coarse copy",
        );
        assert!(
            fitted.roads[1].vertices.is_empty(),
            "primary roads wait for the closest zoom",
        );
        let detailed = at(1.6);
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
            at(2.8).roads[1].vertices.is_empty() && at(3.5).roads[1].vertices.is_empty(),
            "2.8× and 3.5× are still short of the primary threshold",
        );
        assert!(
            !at(4.0).roads[1].vertices.is_empty(),
            "the closest zoom draws the primary roads in view",
        );
    }
}
