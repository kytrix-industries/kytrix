use std::{ops::Range, sync::Arc};

use crate::{
    math::{Point, Rect, Size, Transform},
    paint::{
        atlas::AtlasTile,
        canvas::Canvas,
        color::Color,
        geometry::{FULL_INDEX_RANGE, GeometryId},
    },
};

#[derive(Clone, Debug, PartialEq)]
pub enum Primitive {
    Quad(Quad),
    GlyphRun(GlyphRun),
    Image(Image),
    Path(Path),
    Shadow(Shadow),
    Clip(ClipOp),
    Geometry(GeometryDraw),
    Canvas(Arc<Canvas>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Quad {
    pub bounds: Rect,
    pub corner_radius: [f32; 4],
    pub paint: QuadPaint,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QuadPaint {
    Solid { fill: Color, border: Option<Border> },
    Gradient(Gradient),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Border {
    pub width: f32,
    pub color: Color,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gradient {
    pub from: Color,
    pub to: Color,
    pub kind: GradientKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GradientKind {
    Horizontal,
    Vertical,
    Radial,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GlyphRun {
    pub color: Color,
    pub glyphs: Vec<PositionedGlyph>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    pub tile: AtlasTile,
    pub position: Point,
    pub size: Size,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Image {
    pub bounds: Rect,
    pub tile: AtlasTile,
    pub modulation: Color,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Path {
    pub bounds: Rect,
    pub geometry: GeometryId,
    pub fill: Color,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    pub rect: Rect,
    pub offset: Point,
    pub blur: f32,
    pub color: Color,
    pub corner_radius: [f32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClipOp {
    Push(ClipRect),
    Pop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipRect {
    pub rect: Rect,
    pub corner_radius: [f32; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeometryDraw {
    pub id: GeometryId,
    pub transform: Transform,
    pub color: Color,
    pub range: Range<u32>,
}

impl GeometryDraw {
    #[must_use]
    pub fn full(id: GeometryId, transform: Transform, color: Color) -> Self {
        Self {
            id,
            transform,
            color,
            range: FULL_INDEX_RANGE,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{mem::size_of, sync::Arc};

    use super::{
        Border, ClipOp, ClipRect, GeometryDraw, GlyphRun, Gradient, GradientKind, Image, Path,
        PositionedGlyph, Primitive, Quad, QuadPaint, Shadow,
    };
    use crate::{
        math::{Point, Rect, Size, Transform},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            canvas::Canvas,
            color::Color,
            geometry::GeometryId,
        },
    };

    fn rect() -> Rect {
        Rect::new(0.0, 0.0, 16.0, 8.0)
    }

    fn tile(atlas: AtlasKind) -> AtlasTile {
        AtlasTile {
            atlas,
            page: 0,
            u0: 0.0,
            v0: 0.0,
            u1: 0.5,
            v1: 0.5,
            generation: 1,
            slot: 0,
            slot_epoch: 0,
        }
    }

    fn geometry_id() -> GeometryId {
        GeometryId::for_test(3, 1)
    }

    fn samples() -> Vec<(&'static str, Primitive)> {
        vec![
            (
                "Quad",
                Primitive::Quad(Quad {
                    bounds: rect(),
                    corner_radius: [1.0, 2.0, 3.0, 4.0],
                    paint: QuadPaint::Solid {
                        fill: Color::WHITE,
                        border: Some(Border {
                            width: 1.0,
                            color: Color::BLACK,
                        }),
                    },
                }),
            ),
            (
                "GlyphRun",
                Primitive::GlyphRun(GlyphRun {
                    color: Color::BLACK,
                    glyphs: vec![PositionedGlyph {
                        tile: tile(AtlasKind::Glyph),
                        position: Point::new(1.0, 2.0),
                        size: Size::new(4.0, 6.0),
                    }],
                }),
            ),
            (
                "Image",
                Primitive::Image(Image {
                    bounds: rect(),
                    tile: tile(AtlasKind::Image),
                    modulation: Color::WHITE,
                }),
            ),
            (
                "Path",
                Primitive::Path(Path {
                    bounds: rect(),
                    geometry: geometry_id(),
                    fill: Color::WHITE,
                }),
            ),
            (
                "Shadow",
                Primitive::Shadow(Shadow {
                    rect: rect(),
                    offset: Point::new(0.0, 2.0),
                    blur: 4.0,
                    color: Color::BLACK,
                    corner_radius: [1.0, 2.0, 3.0, 4.0],
                }),
            ),
            (
                "Clip",
                Primitive::Clip(ClipOp::Push(ClipRect {
                    rect: rect(),
                    corner_radius: [0.0; 4],
                })),
            ),
            (
                "Geometry",
                Primitive::Geometry(GeometryDraw::full(
                    geometry_id(),
                    Transform::IDENTITY,
                    Color::WHITE,
                )),
            ),
            ("Canvas", Primitive::Canvas(Arc::new(Canvas::new(rect())))),
        ]
    }

    #[test]
    fn all_variants_clone_eq_and_debug() {
        let samples = samples();
        assert_eq!(samples.len(), 8);
        for (name, sample) in &samples {
            let clone = sample.clone();
            assert_eq!(&clone, sample, "variant {name}");
            let text = format!("{sample:?}");
            assert!(text.contains(name), "variant {name} missing from {text}");
        }
        match &samples[0].1 {
            Primitive::Quad(quad) => {
                let mut different = quad.clone();
                different.corner_radius[0] = 99.0;
                assert_ne!(&Primitive::Quad(different), &samples[0].1);
            }
            other => panic!("expected Quad, got {other:?}"),
        }
    }

    #[test]
    fn size_budget() {
        assert_eq!(size_of::<Primitive>(), 80);
        assert!(size_of::<Primitive>() <= 128);
    }

    #[test]
    fn geometry_draw_range_is_part_of_the_value() {
        let full = GeometryDraw::full(geometry_id(), Transform::IDENTITY, Color::WHITE);
        assert_eq!(
            full.range,
            0..u32::MAX,
            "`full` 必须用整份哨兵（几何可增长）"
        );
        let mut subset = full.clone();
        subset.range = 3..9;
        assert_ne!(subset, full, "不同子区间是不同的绘制（参与相等性）");
        let mut empty = subset.clone();
        empty.range = 6..6;
        assert_ne!(empty, subset, "空区间与子区间不同");
    }

    #[test]
    fn quad_paint_encodings_are_mutually_exclusive() {
        let plain = QuadPaint::Solid {
            fill: Color::WHITE,
            border: None,
        };
        let bordered = QuadPaint::Solid {
            fill: Color::WHITE,
            border: Some(Border {
                width: 1.0,
                color: Color::BLACK,
            }),
        };
        let gradient = QuadPaint::Gradient(Gradient {
            from: Color::WHITE,
            to: Color::BLACK,
            kind: GradientKind::Horizontal,
        });
        assert_ne!(plain, bordered);
        assert_ne!(bordered, gradient);
        assert_ne!(plain, gradient);
        assert!(format!("{plain:?}").contains("Solid"));
        assert!(format!("{gradient:?}").contains("Gradient"));
        match gradient {
            QuadPaint::Solid { .. } => panic!("expected gradient"),
            QuadPaint::Gradient(Gradient { kind, .. }) => {
                assert_eq!(kind, GradientKind::Horizontal)
            }
        }
    }

    #[test]
    fn shadow_keeps_unoffset_rect() {
        let rect = Rect::new(4.0, 4.0, 8.0, 8.0);
        let shadow = Shadow {
            rect,
            offset: Point::new(10.0, 20.0),
            blur: 2.0,
            color: Color::BLACK,
            corner_radius: [0.0; 4],
        };
        assert_eq!(shadow.rect, rect);
    }

    #[test]
    fn gradient_kind_has_three_distinct_variants() {
        let kinds = [
            GradientKind::Horizontal,
            GradientKind::Vertical,
            GradientKind::Radial,
        ];
        for (index, left) in kinds.iter().enumerate() {
            for right in &kinds[index + 1..] {
                assert_ne!(left, right);
            }
            assert!(!format!("{left:?}").is_empty());
        }
        assert_eq!(format!("{:?}", GradientKind::Radial), "Radial");
    }

    #[test]
    fn clip_op_push_pop_roundtrip() {
        let clip = ClipRect {
            rect: rect(),
            corner_radius: [2.0, 0.0, 2.0, 0.0],
        };
        assert_eq!(ClipOp::Push(clip), ClipOp::Push(clip));
        assert_ne!(ClipOp::Pop, ClipOp::Push(clip));
        let other = ClipRect {
            rect: rect(),
            corner_radius: [0.0; 4],
        };
        assert_ne!(ClipOp::Push(clip), ClipOp::Push(other));
        assert!(format!("{:?}", ClipOp::Push(clip)).contains("Push"));
        assert!(format!("{:?}", ClipOp::Pop).contains("Pop"));
    }
}
