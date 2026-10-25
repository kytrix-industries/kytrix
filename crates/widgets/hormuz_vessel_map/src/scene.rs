//! The land under the vessels: the packaged coastline and its triangles.
//!
//! The coastline is a checked-in binary ([`LAND_ASSET`]) because it is data,
//! not an icon: one file, read once, embedded by [`include_bytes!`]. Loading it
//! is the only expensive step, and it happens once per process:
//!
//! 1. [`Scene::parse`] walks the KHL1 rings, projects each vertex from degrees
//!    to Web Mercator, and tessellates the whole set as one even-odd fill, so
//!    lakes and seas cut holes in the land instead of being filled over.
//! 2. It then keeps the resulting triangles as raw `[f32; 6]` rows (three
//!    `x, y` pairs) in Mercator space, plus the extent they cover.
//!
//! The frame only applies the camera's affine transform to those rows. It must
//! not tessellate again: rebuilding the path costs several milliseconds while
//! emitting the cached triangles costs a fraction of one.

use std::sync::OnceLock;

use gpui_kit::{FillOptions, FillRule, PathBuilder, PathStyle, point, px};

use crate::model;

/// The packaged coastline: KHL1 rings of `f32` longitude/latitude pairs.
///
/// See `assets/README.md` for where it came from and how it was packed.
const LAND_ASSET: &[u8] = include_bytes!("../assets/hormuz_land.bin");

/// The four bytes a KHL1 asset starts with.
const MAGIC: [u8; 4] = *b"KHL1";

/// `magic | ring_count | vert_count`, each count a little-endian `u32`.
const HEADER_LEN: usize = 12;

/// One longitude/latitude pair, each a little-endian `f32`.
const VERTEX_LEN: usize = 8;

/// A ring needs at least the three points of a triangle.
const MIN_RING_POINTS: u32 = 3;

/// The flattening tolerance handed to lyon, in Mercator radians.
///
/// lyon uses this for more than curve flattening: its fill tessellator also
/// treats vertices closer together than half of the tolerance as connected.
/// The value is absolute, and the whole map is only about `0.13` Mercator
/// radians tall, so the default of `0.1` merges distinct coastline points: it
/// drops 21 of the 6,015 distinct vertices and yields 5,520 triangles.
/// `5e-6` is roughly 32 m at the equator — well under one pixel even at the
/// map's maximum zoom of 4× — so no vertex in the asset is collapsed: all
/// 6,015 distinct points still appear in the triangles. The tests pin the
/// triangle count this produces, because changing the tolerance changes how
/// lyon joins nearly touching edges.
const TESSELLATION_TOLERANCE: f32 = 5e-6;

/// Why the packaged coastline could not be turned into a [`Scene`].
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SceneError {
    /// The asset does not start with [`MAGIC`].
    #[error("map asset has an invalid magic")]
    InvalidMagic,
    /// The asset ends before a field or a vertex it declares.
    #[error("map asset is truncated")]
    Truncated,
    /// A ring declares fewer points than a triangle or more than are left.
    #[error("map asset has an invalid ring point count")]
    PointCountOverflow,
    /// The rings hold fewer or more vertices than the header declares.
    #[error("map asset vertex count does not match its rings")]
    VertexCountMismatch,
    /// The asset continues after the last vertex its rings declare.
    #[error("map asset has trailing bytes")]
    TrailingBytes,
    /// The asset tessellated into no triangles at all.
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

/// The coastline, tessellated once and kept in Web Mercator space.
pub struct Scene {
    /// Triangle rows: three `x, y` pairs each, in Mercator radians.
    land_triangles: Vec<[f32; 6]>,
    /// The extent the triangles cover, as `[min, max]` in Mercator radians.
    bounds: [[f32; 2]; 2],
}

impl Scene {
    /// Parses and tessellates a KHL1 asset.
    ///
    /// Every structural error is caught before the tessellator runs, so a
    /// malformed asset never reaches lyon. What remains is lyon rejecting the
    /// geometry, or rings that enclose no area.
    pub fn parse(bytes: &[u8]) -> Result<Self, SceneError> {
        if bytes.len() < MAGIC.len() {
            return Err(SceneError::Truncated);
        }
        if bytes[..MAGIC.len()] != MAGIC {
            return Err(SceneError::InvalidMagic);
        }
        if bytes.len() < HEADER_LEN {
            return Err(SceneError::Truncated);
        }

        let ring_count = read_u32(bytes, 4);
        let vertex_count = read_u32(bytes, 8);
        let mut offset = HEADER_LEN;
        let mut remaining_vertices = vertex_count;
        let mut bounds = [[f32::INFINITY; 2], [f32::NEG_INFINITY; 2]];

        let options = FillOptions::default()
            .with_fill_rule(FillRule::EvenOdd)
            .with_tolerance(TESSELLATION_TOLERANCE);
        let mut builder = PathBuilder::fill().with_style(PathStyle::Fill(options));

        for _ in 0..ring_count {
            if bytes.len() - offset < size_of::<u32>() {
                return Err(SceneError::Truncated);
            }
            let point_count = read_u32(bytes, offset);
            offset += size_of::<u32>();
            if point_count < MIN_RING_POINTS || point_count > remaining_vertices {
                return Err(SceneError::PointCountOverflow);
            }
            let ring_len = u64::from(point_count) * VERTEX_LEN as u64;
            if ring_len > (bytes.len() - offset) as u64 {
                return Err(SceneError::Truncated);
            }
            remaining_vertices -= point_count;

            for index in 0..point_count {
                let vertex = offset + index as usize * VERTEX_LEN;
                let longitude = f32::from_le_bytes(read_array(bytes, vertex));
                let latitude = f32::from_le_bytes(read_array(bytes, vertex + 4));
                let [x, y] = model::project(longitude, latitude);
                if !x.is_finite() || !y.is_finite() {
                    return Err(SceneError::NonFiniteCoordinate);
                }
                bounds[0][0] = bounds[0][0].min(x);
                bounds[0][1] = bounds[0][1].min(y);
                bounds[1][0] = bounds[1][0].max(x);
                bounds[1][1] = bounds[1][1].max(y);
                let at = point(px(x), px(y));
                if index == 0 {
                    builder.move_to(at);
                } else {
                    builder.line_to(at);
                }
            }
            builder.close();
            offset += point_count as usize * VERTEX_LEN;
        }

        if offset != bytes.len() {
            return Err(SceneError::TrailingBytes);
        }
        if remaining_vertices != 0 {
            return Err(SceneError::VertexCountMismatch);
        }

        let path = builder.build().map_err(|_| SceneError::Tessellation)?;
        let mut land_triangles = Vec::with_capacity(path.vertices.len() / 3);
        for vertices in path.vertices.as_chunks::<3>().0 {
            land_triangles.push([
                f32::from(vertices[0].xy_position.x),
                f32::from(vertices[0].xy_position.y),
                f32::from(vertices[1].xy_position.x),
                f32::from(vertices[1].xy_position.y),
                f32::from(vertices[2].xy_position.x),
                f32::from(vertices[2].xy_position.y),
            ]);
        }
        if land_triangles.is_empty() {
            return Err(SceneError::Empty);
        }

        Ok(Self {
            land_triangles,
            bounds,
        })
    }

    /// The land as triangle rows of three `x, y` Mercator pairs each.
    pub fn land_triangles(&self) -> &[[f32; 6]] {
        &self.land_triangles
    }

    /// The extent the land covers, as `[min, max]` in Mercator radians.
    pub fn bounds(&self) -> [[f32; 2]; 2] {
        self.bounds
    }
}

/// The one [`Scene`] of this process, parsed on first use.
///
/// A failure is cached too: the asset is compiled in, so retrying would walk
/// the same bytes and reach the same error. Callers get a clone of it.
pub fn scene() -> Result<&'static Scene, SceneError> {
    static SCENE: OnceLock<Result<Scene, SceneError>> = OnceLock::new();
    SCENE
        .get_or_init(|| Scene::parse(LAND_ASSET))
        .as_ref()
        .map_err(Clone::clone)
}

/// Reads a little-endian `u32` at `offset`; the caller checked the length.
fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(read_array(bytes, offset))
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

    /// The ring count the design was measured with.
    const RING_COUNT: u32 = 251;
    /// The vertex count the design was measured with.
    const VERTEX_COUNT: u32 = 6_266;
    /// The triangles those rings tessellate into.
    const TRIANGLE_COUNT: usize = 5_518;
    /// Coordinates are compared in Mercator radians; a few ulp is enough.
    const EPSILON: f32 = 1e-6;

    fn asset_u32(offset: usize) -> u32 {
        read_u32(LAND_ASSET, offset)
    }

    /// A minimal valid KHL1 asset: one ring, declared as closed.
    fn ring_asset(points: &[[f32; 2]]) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(points.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(points.len() as u32).to_le_bytes());
        for [longitude, latitude] in points {
            bytes.extend_from_slice(&longitude.to_le_bytes());
            bytes.extend_from_slice(&latitude.to_le_bytes());
        }
        bytes
    }

    /// A minimal valid KHL1 asset: one triangle ring, closed.
    fn triangle_asset() -> Vec<u8> {
        ring_asset(&[[50.0, 25.0], [51.0, 25.0], [50.0, 26.0], [50.0, 25.0]])
    }

    #[test]
    fn the_asset_declares_the_measured_shape() {
        assert_eq!(&LAND_ASSET[..MAGIC.len()], &MAGIC);
        assert_eq!(asset_u32(4), RING_COUNT);
        assert_eq!(asset_u32(8), VERTEX_COUNT);

        let scene = Scene::parse(LAND_ASSET).expect("the checked-in asset parses");
        assert_eq!(scene.land_triangles().len(), TRIANGLE_COUNT);
    }

    #[test]
    fn the_land_covers_the_map_box_and_no_more() {
        let [min, max] = scene().expect("the checked-in asset parses").bounds();
        let [west, south, east, north] = model::mercator_bounds();
        assert!((min[0] - west).abs() <= EPSILON);
        assert!((min[1] - south).abs() <= EPSILON);
        assert!((max[0] - east).abs() <= EPSILON);
        assert!((max[1] - north).abs() <= EPSILON);
    }

    #[test]
    fn every_triangle_is_finite_and_inside_the_map_box() {
        let scene = scene().expect("the checked-in asset parses");
        let [min, max] = scene.bounds();
        for triangle in scene.land_triangles() {
            for pair in triangle.as_chunks::<2>().0 {
                let [x, y] = *pair;
                assert!(x.is_finite() && y.is_finite());
                assert!(x >= min[0] - EPSILON && x <= max[0] + EPSILON);
                assert!(y >= min[1] - EPSILON && y <= max[1] + EPSILON);
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
    fn every_truncated_prefix_is_rejected() {
        let bytes = triangle_asset();
        assert_eq!(
            Scene::parse(&bytes).map(|scene| scene.land_triangles().len()),
            Ok(1)
        );
        for cut in 0..bytes.len() {
            assert_eq!(
                Scene::parse(&bytes[..cut]).err(),
                Some(SceneError::Truncated),
                "a prefix of {cut} bytes should be truncated"
            );
        }
    }

    #[test]
    fn a_ring_point_count_beyond_the_declared_vertices_is_rejected() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8 * 5]);
        assert_eq!(
            Scene::parse(&bytes).err(),
            Some(SceneError::PointCountOverflow)
        );
    }

    #[test]
    fn a_ring_of_two_points_is_rejected() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 8 * 2]);
        assert_eq!(
            Scene::parse(&bytes).err(),
            Some(SceneError::PointCountOverflow)
        );
    }

    #[test]
    fn a_vertex_count_that_does_not_match_the_rings_is_rejected() {
        let mut bytes = LAND_ASSET.to_vec();
        bytes[8..12].copy_from_slice(&(VERTEX_COUNT + 1).to_le_bytes());
        assert_eq!(
            Scene::parse(&bytes).err(),
            Some(SceneError::VertexCountMismatch)
        );
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = LAND_ASSET.to_vec();
        bytes.push(0);
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::TrailingBytes));
    }

    #[test]
    fn an_asset_without_rings_is_empty() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(Scene::parse(&bytes).err(), Some(SceneError::Empty));
    }

    #[test]
    fn an_asset_that_tessellates_to_nothing_is_empty() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&3u32.to_le_bytes());
        for _ in 0..3 {
            bytes.extend_from_slice(&50.0f32.to_le_bytes());
            bytes.extend_from_slice(&25.0f32.to_le_bytes());
        }
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
                Some(SceneError::NonFiniteCoordinate)
            );
        }
    }
}
