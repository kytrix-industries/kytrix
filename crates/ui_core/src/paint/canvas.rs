use std::sync::Arc;

use crate::{
    math::{LogicalPoint, LogicalPx, LogicalRect},
    paint::{
        atlas::AtlasKind,
        primitive::{
            ClipOp, ClipRect, GeometryDraw, GlyphRun, Image, Path, Primitive, Quad, Shadow,
        },
        scene::{Lane, MAX_CLIP_DEPTH},
    },
    text::TextContent,
};

fn is_well_formed(clip: &ClipRect) -> bool {
    let rect = clip.rect;
    rect.origin.x.is_finite()
        && rect.origin.y.is_finite()
        && rect.size.width.is_finite()
        && rect.size.height.is_finite()
        && rect.size.width >= LogicalPx::ZERO
        && rect.size.height >= LogicalPx::ZERO
        && clip.corner_radius.iter().all(|radius| radius.is_finite())
}

#[derive(Clone, Debug, PartialEq)]
pub struct CanvasText {
    pub content: TextContent,
    pub origin: LogicalPoint,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Canvas {
    bounds: LogicalRect,
    lane: Lane,
}

impl Canvas {
    #[must_use]
    pub fn new(bounds: LogicalRect) -> Self {
        Self {
            bounds,
            lane: Lane::new(),
        }
    }

    pub(crate) fn push_primitive(&mut self, primitive: Primitive) {
        self.lane.push(primitive);
    }

    pub fn push_quad(&mut self, quad: Quad) {
        self.lane.push(Primitive::Quad(quad));
    }

    #[must_use]
    pub fn push_text(&mut self, content: TextContent, origin: LogicalPoint) -> bool {
        if content.text.is_empty() {
            return false;
        }
        self.lane
            .push(Primitive::Text(Arc::new(CanvasText { content, origin })));
        true
    }

    pub(crate) fn push_glyph_run(&mut self, run: GlyphRun) {
        debug_assert!(
            run.glyphs
                .iter()
                .all(|glyph| glyph.tile.atlas.is_glyph() && glyph.tile.page == 0),
            "glyph run contains a tile from a non-glyph atlas or non-zero layer"
        );
        self.lane.push(Primitive::GlyphRun(run));
    }

    pub fn push_image(&mut self, image: Image) {
        debug_assert!(
            image.tile.atlas == AtlasKind::Image,
            "image tile must come from the image atlas"
        );
        debug_assert!(
            image.tile.page < crate::paint::atlas::MAX_IMAGE_PAGES,
            "image tile page {} out of range (MAX_IMAGE_PAGES={})",
            image.tile.page,
            crate::paint::atlas::MAX_IMAGE_PAGES
        );
        self.lane.push(Primitive::Image(image));
    }

    pub fn push_path(&mut self, path: Path) {
        self.lane.push(Primitive::Path(path));
    }

    pub fn push_shadow(&mut self, shadow: Shadow) {
        self.lane.push(Primitive::Shadow(shadow));
    }

    pub fn push_geometry(&mut self, draw: GeometryDraw) {
        self.lane.push(Primitive::Geometry(draw));
    }

    #[must_use]
    pub fn push_clip(&mut self, clip: ClipRect) -> bool {
        debug_assert!(
            is_well_formed(&clip),
            "clip rect must be finite with non-negative size"
        );
        if self.lane.rounded_clip_depth() >= usize::from(MAX_CLIP_DEPTH) {
            return false;
        }
        self.lane.push(Primitive::Clip(ClipOp::Push(clip)));
        true
    }

    #[must_use]
    pub fn pop_clip(&mut self) -> bool {
        if self.lane.clip_depth() == 0 {
            return false;
        }
        self.lane.push(Primitive::Clip(ClipOp::Pop));
        true
    }

    #[must_use]
    pub fn bounds(&self) -> LogicalRect {
        self.bounds
    }

    #[must_use]
    pub fn primitives(&self) -> &[Primitive] {
        self.lane.primitives()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lane.is_empty()
    }
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new(LogicalRect::default())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{Canvas, CanvasText};
    use crate::{
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::Color,
            primitive::{
                CanvasDraw, ClipRect, GlyphRun, Image, PositionedGlyph, Primitive, Quad, QuadPaint,
                Shadow,
            },
            scene::MAX_CLIP_DEPTH,
        },
        text::{TextContent, TextStyle},
    };

    fn bounds() -> LogicalRect {
        LogicalRect::new(px(0.0), px(0.0), px(64.0), px(32.0))
    }

    fn quad() -> Quad {
        Quad {
            bounds: bounds(),
            corner_radius: [px(0.0); 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        }
    }

    fn clip() -> ClipRect {
        ClipRect {
            rect: bounds(),
            corner_radius: [px(0.0); 4],
        }
    }

    fn rounded_clip() -> ClipRect {
        ClipRect {
            rect: bounds(),
            corner_radius: [px(4.0); 4],
        }
    }

    fn image() -> Image {
        Image {
            bounds: bounds(),
            tile: AtlasTile {
                atlas: AtlasKind::Image,
                page: 0,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                generation: 0,
                slot: 0,
                slot_epoch: 0,
            },
            modulation: Color::WHITE,
        }
    }

    fn shadow() -> Shadow {
        Shadow {
            rect: bounds(),
            offset: LogicalPoint::new(px(0.0), px(1.0)),
            blur: px(2.0),
            color: Color::BLACK,
            corner_radius: [px(0.0); 4],
        }
    }

    fn glyph_run() -> GlyphRun {
        GlyphRun {
            color: Color::BLACK,
            glyphs: vec![PositionedGlyph {
                tile: AtlasTile {
                    atlas: AtlasKind::Glyph,
                    page: 0,
                    u0: 0.0,
                    v0: 0.0,
                    u1: 0.5,
                    v1: 0.5,
                    generation: 0,
                    slot: 0,
                    slot_epoch: 0,
                },
                position: LogicalPoint::new(px(2.0), px(3.0)),
                size: LogicalSize::new(px(8.0), px(8.0)),
            }],
        }
    }

    #[test]
    fn new_canvas_is_empty() {
        let canvas = Canvas::new(bounds());
        assert!(canvas.is_empty());
        assert!(canvas.primitives().is_empty());
        assert_eq!(canvas.bounds(), bounds());
    }

    #[test]
    fn default_canvas_is_empty() {
        let canvas = Canvas::default();
        assert_eq!(canvas, Canvas::new(LogicalRect::default()));
        assert!(canvas.is_empty());
    }

    #[test]
    fn push_orders_primitives() {
        let mut canvas = Canvas::new(bounds());
        canvas.push_quad(quad());
        canvas.push_image(image());
        canvas.push_shadow(shadow());
        let kinds: Vec<&str> = canvas
            .primitives()
            .iter()
            .map(|primitive| match primitive {
                Primitive::Quad(_) => "quad",
                Primitive::Image(_) => "image",
                Primitive::Shadow(_) => "shadow",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["quad", "image", "shadow"]);
    }

    #[test]
    fn clip_push_pop_balance() {
        let mut canvas = Canvas::new(bounds());
        canvas.push_quad(quad());
        assert!(canvas.push_clip(clip()));
        assert!(canvas.pop_clip());
        assert_eq!(canvas.primitives().len(), 3);
    }

    #[test]
    fn push_clip_beyond_limit_returns_false() {
        let mut canvas = Canvas::new(bounds());
        for _ in 0..MAX_CLIP_DEPTH {
            assert!(canvas.push_clip(rounded_clip()));
        }
        let before = canvas.primitives().len();
        assert!(!canvas.push_clip(rounded_clip()));
        assert_eq!(canvas.primitives().len(), before);
        assert!(canvas.pop_clip());
        assert!(canvas.push_clip(rounded_clip()));
    }

    #[test]
    fn rect_clips_do_not_consume_the_rounded_budget() {
        let mut canvas = Canvas::new(bounds());
        for _ in 0..12 {
            assert!(canvas.push_clip(clip()), "直角矩形 clip 不占圆角预算");
        }
        for _ in 0..MAX_CLIP_DEPTH {
            assert!(canvas.push_clip(rounded_clip()));
        }
        assert!(!canvas.push_clip(rounded_clip()), "圆角层不能超过预算");
    }

    #[test]
    fn pop_clip_on_empty_returns_false() {
        let mut canvas = Canvas::new(bounds());
        assert!(!canvas.pop_clip());
        assert!(canvas.is_empty());
    }

    #[test]
    fn push_text_records_only_the_logical_recipe() {
        let mut canvas = Canvas::new(bounds());
        let content = TextContent::new("42", TextStyle::new(px(12.0)));
        let origin = LogicalPoint::new(px(6.0), px(8.0));
        assert!(canvas.push_text(content.clone(), origin));
        assert_eq!(
            canvas.primitives(),
            &[Primitive::Text(Arc::new(CanvasText { content, origin }))]
        );
        assert_eq!(canvas.primitives().len(), 1);
    }

    #[test]
    fn push_text_refuses_empty_content() {
        let mut canvas = Canvas::new(bounds());
        assert!(!canvas.push_text(
            TextContent::new("", TextStyle::new(px(12.0))),
            LogicalPoint::ZERO
        ));
        assert!(canvas.is_empty());
    }

    #[test]
    fn push_glyph_run_keeps_run() {
        let mut canvas = Canvas::new(bounds());
        let original = glyph_run();
        canvas.push_glyph_run(original.clone());
        match &canvas.primitives()[0] {
            Primitive::GlyphRun(run) => {
                assert_eq!(run, &original);
                assert_eq!(run.color, original.color);
                assert_eq!(run.glyphs[0].tile, original.glyphs[0].tile);
                assert_eq!(run.glyphs[0].position, original.glyphs[0].position);
                assert_eq!(run.glyphs[0].size, original.glyphs[0].size);
            }
            other => panic!("expected GlyphRun, got {other:?}"),
        }
    }

    #[test]
    fn push_primitive_keeps_nested_canvas() {
        let mut inner = Canvas::new(bounds());
        inner.push_quad(quad());
        let mut canvas = Canvas::new(bounds());
        canvas.push_primitive(Primitive::Canvas(CanvasDraw::new(Arc::new(inner))));
        assert!(matches!(
            canvas.primitives()[0],
            Primitive::Canvas(ref nested) if nested.primitives().len() == 1
        ));
    }

    #[test]
    fn canvas_size_is_48_bytes() {
        assert_eq!(std::mem::size_of::<Canvas>(), 48);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "image tile must come from the image atlas")]
    fn push_image_rejects_glyph_atlas() {
        let mut canvas = Canvas::new(bounds());
        let mut image = image();
        image.tile.atlas = AtlasKind::Glyph;
        canvas.push_image(image);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "clip rect must be finite with non-negative size")]
    fn push_clip_rejects_negative_size() {
        let mut canvas = Canvas::new(bounds());
        let mut clip = clip();
        clip.rect = LogicalRect::new(px(0.0), px(0.0), px(-1.0), px(8.0));
        let _ = canvas.push_clip(clip);
    }

    #[test]
    fn canvas_is_clone_and_eq() {
        let mut canvas = Canvas::new(bounds());
        canvas.push_quad(quad());
        let clone = canvas.clone();
        assert_eq!(clone, canvas);
    }

    #[test]
    #[should_panic(expected = "image tile page")]
    fn push_image_rejects_out_of_range_atlas_page() {
        let mut canvas = Canvas::new(bounds());
        let mut out_of_range = image();
        out_of_range.tile.page = crate::paint::atlas::MAX_IMAGE_PAGES;
        canvas.push_image(out_of_range);
    }

    #[test]
    #[should_panic(expected = "glyph run contains a tile")]
    fn push_glyph_run_rejects_non_zero_glyph_atlas_page() {
        let mut canvas = Canvas::new(bounds());
        let mut run = glyph_run();
        run.glyphs[0].tile.page = 1;
        canvas.push_glyph_run(run);
    }
}
