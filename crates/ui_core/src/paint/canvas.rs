use std::sync::Arc;

use crate::{
    math::{LogicalPoint, LogicalPx, LogicalRect},
    paint::{
        atlas::AtlasKind,
        primitive::{
            ClipOp, ClipRect, GeometryDraw, GlyphRun, Image, Path, PositionedGlyph, Primitive,
            Quad, Shadow,
        },
        scene::{Lane, MAX_CLIP_DEPTH},
    },
    render::AtlasAllocator,
    text::{ShapedTextHandle, ShapedTextItem},
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

    pub fn push_quad(&mut self, quad: Quad) {
        self.lane.push(Primitive::Quad(quad));
    }

    pub fn push_text(&mut self, run: GlyphRun) {
        debug_assert!(
            run.glyphs
                .iter()
                .all(|glyph| glyph.tile.atlas == AtlasKind::Glyph && glyph.tile.page == 0),
            "glyph run contains a tile from a non-glyph atlas or non-zero layer"
        );
        self.lane.push(Primitive::GlyphRun(run));
    }

    #[must_use]
    pub fn push_shaped_text(&mut self, handle: &ShapedTextHandle, origin: LogicalPoint) -> bool {
        if handle.is_stale() {
            return false;
        }
        for shaped_line in &handle.line().lines {
            for item in &shaped_line.items {
                let ShapedTextItem::GlyphRun(run) = item else {
                    continue;
                };
                if run.glyphs.is_empty() {
                    continue;
                }
                let mut glyphs = Vec::with_capacity(run.glyphs.len());
                for glyph in &run.glyphs {
                    glyphs.push(PositionedGlyph {
                        tile: glyph.tile,
                        position: LogicalPoint::new(
                            origin.x + glyph.position.x,
                            origin.y + glyph.position.y,
                        ),
                        size: glyph.size,
                    });
                }
                self.push_text(GlyphRun {
                    color: run.color,
                    glyphs,
                });
            }
        }
        true
    }

    #[must_use]
    pub(crate) fn without_stale_glyph_runs(
        &self,
        atlas: &AtlasAllocator,
    ) -> Option<(Canvas, usize)> {
        let mut filtered = Lane::new();
        let mut dropped = 0usize;
        let mut changed = false;
        for primitive in self.lane.primitives() {
            match primitive {
                Primitive::GlyphRun(run) => {
                    if run.glyphs.iter().any(|glyph| !atlas.is_alive(&glyph.tile)) {
                        dropped += 1;
                        changed = true;
                    } else {
                        filtered.push(primitive.clone());
                    }
                }
                Primitive::Canvas(inner) => match inner.without_stale_glyph_runs(atlas) {
                    Some((sanitized, inner_dropped)) => {
                        dropped += inner_dropped;
                        changed = true;
                        filtered.push(Primitive::Canvas(Arc::new(sanitized)));
                    }
                    None => filtered.push(primitive.clone()),
                },
                _ => filtered.push(primitive.clone()),
            }
        }
        if !changed {
            return None;
        }
        Some((
            Canvas {
                bounds: self.bounds,
                lane: filtered,
            },
            dropped,
        ))
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
    use super::Canvas;
    use crate::{
        math::{DevicePixelRatio, LogicalPoint, LogicalRect, LogicalSize, px},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::Color,
            primitive::{
                ClipOp, ClipRect, GlyphRun, Image, PositionedGlyph, Primitive, Quad, QuadPaint,
                Shadow,
            },
            scene::MAX_CLIP_DEPTH,
        },
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
    fn push_text_keeps_run() {
        let mut canvas = Canvas::new(bounds());
        let original = glyph_run();
        canvas.push_text(original.clone());
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
    fn push_text_rejects_non_zero_glyph_atlas_page() {
        let mut canvas = Canvas::new(bounds());
        let mut run = glyph_run();
        run.glyphs[0].tile.page = 1;
        canvas.push_text(run);
    }

    fn shaped_handle(
        generation: u32,
        eviction_epoch: u32,
        current: (u32, u32),
    ) -> crate::text::ShapedTextHandle {
        use std::sync::{Arc, atomic::AtomicU64};

        use crate::text::{
            ShapedGlyphRun, ShapedLine, ShapedTextHandle, ShapedTextItem, ShapedTextLine,
            pack_atlas_lease,
        };

        let glyph = PositionedGlyph {
            tile: AtlasTile {
                atlas: AtlasKind::Glyph,
                page: 0,
                u0: 0.0,
                v0: 0.0,
                u1: 0.5,
                v1: 0.5,
                generation,
                slot: 0,
                slot_epoch: 0,
            },
            position: LogicalPoint::new(px(2.0), px(3.0)),
            size: LogicalSize::new(px(8.0), px(8.0)),
        };
        let line = ShapedLine {
            text: smol_str::SmolStr::new_static("e1"),
            lines: vec![ShapedTextLine {
                text_range: 0..2,
                baseline: px(12.0),
                line_height: px(14.0),
                advance: px(10.0),
                items: vec![ShapedTextItem::GlyphRun(ShapedGlyphRun {
                    color: Color::BLACK,
                    text_range: 0..2,
                    glyphs: vec![glyph],
                })],
                decorations: Vec::new(),
            }],
            width: px(10.0),
            height: px(14.0),
            first_baseline: Some(px(12.0)),
            atlas_generation: generation,
            font_stack_version: 1,
            scale: DevicePixelRatio::ONE,
        };
        ShapedTextHandle::new(
            Arc::new(line),
            Box::new([]),
            generation,
            eviction_epoch,
            DevicePixelRatio::ONE,
            Arc::new(AtomicU64::new(pack_atlas_lease(current.0, current.1))),
        )
    }

    #[test]
    fn push_shaped_text_offsets_glyphs_by_the_origin() {
        let mut canvas = Canvas::new(bounds());
        let handle = shaped_handle(7, 2, (7, 2));
        assert!(
            canvas.push_shaped_text(&handle, LogicalPoint::new(px(10.0), px(20.0))),
            "当前代际的句柄必须能编码"
        );
        assert_eq!(canvas.primitives().len(), 1);
        let Primitive::GlyphRun(run) = &canvas.primitives()[0] else {
            panic!("push_shaped_text 只能产生 GlyphRun 图元");
        };
        let glyph = run.glyphs[0];
        assert_eq!(
            glyph.position,
            LogicalPoint::new(px(10.0 + 2.0), px(20.0 + 3.0)),
            "origin 是文本盒左上角的绝对窗口逻辑像素"
        );
        assert_eq!(glyph.size, LogicalSize::new(px(8.0), px(8.0)));
        assert_eq!(glyph.tile.generation, 7);
        assert_eq!(run.color, Color::BLACK);
    }

    #[test]
    fn push_shaped_text_refuses_a_stale_handle() {
        let mut canvas = Canvas::new(bounds());
        canvas.push_quad(quad());
        let before = canvas.primitives().len();
        let stale = shaped_handle(7, 0, (8, 0));
        assert!(stale.is_stale());
        assert!(
            !canvas.push_shaped_text(&stale, LogicalPoint::ZERO),
            "stale 句柄必须返回 false"
        );
        assert_eq!(
            canvas.primitives().len(),
            before,
            "stale push 不得产生任何图元（不画旧 UV）"
        );
        let evicted = shaped_handle(7, 2, (7, 3));
        assert!(
            evicted.is_stale(),
            "淘汰纪元前进必须让句柄失效（tile 已被别的字形复用）"
        );
        assert!(
            !canvas.push_shaped_text(&evicted, LogicalPoint::ZERO),
            "淘汰后的句柄不得编码旧 UV"
        );
        assert_eq!(canvas.primitives().len(), before);
    }

    #[test]
    fn push_shaped_text_splits_multiple_runs_into_multiple_primitives() {
        use std::sync::{Arc, atomic::AtomicU64};

        use crate::text::{
            ShapedGlyphRun, ShapedLine, ShapedTextHandle, ShapedTextItem, ShapedTextLine,
            pack_atlas_lease,
        };

        let run = |x: f32| ShapedGlyphRun {
            color: Color::BLACK,
            text_range: 0..1,
            glyphs: vec![PositionedGlyph {
                tile: AtlasTile {
                    atlas: AtlasKind::Glyph,
                    page: 0,
                    u0: x / 8.0,
                    v0: 0.0,
                    u1: (x + 8.0) / 8.0,
                    v1: 1.0,
                    generation: 3,
                    slot: 0,
                    slot_epoch: 0,
                },
                position: LogicalPoint::new(px(x), px(0.0)),
                size: LogicalSize::new(px(8.0), px(8.0)),
            }],
        };
        let line = ShapedLine {
            text: smol_str::SmolStr::new_static("ab"),
            lines: vec![ShapedTextLine {
                text_range: 0..2,
                baseline: px(12.0),
                line_height: px(14.0),
                advance: px(16.0),
                items: vec![
                    ShapedTextItem::GlyphRun(run(0.0)),
                    ShapedTextItem::InlineBox(crate::text::ShapedInlineBox {
                        id: 5,
                        rect: LogicalRect::new(px(4.0), px(0.0), px(8.0), px(8.0)),
                        baseline: None,
                    }),
                    ShapedTextItem::GlyphRun(run(8.0)),
                ],
                decorations: Vec::new(),
            }],
            width: px(16.0),
            height: px(14.0),
            first_baseline: Some(px(12.0)),
            atlas_generation: 3,
            font_stack_version: 1,
            scale: DevicePixelRatio::ONE,
        };
        let handle = ShapedTextHandle::new(
            Arc::new(line),
            Box::new([]),
            3,
            0,
            DevicePixelRatio::ONE,
            Arc::new(AtomicU64::new(pack_atlas_lease(3, 0))),
        );
        let mut canvas = Canvas::new(bounds());
        assert!(canvas.push_shaped_text(&handle, LogicalPoint::new(px(1.0), px(2.0))));
        assert_eq!(
            canvas.primitives().len(),
            2,
            "按 run 拆图元：行内盒不产生图元（E1 只保证落点可读）"
        );
        let positions: Vec<LogicalPoint> = canvas
            .primitives()
            .iter()
            .filter_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => run.glyphs.first().map(|glyph| glyph.position),
                _ => None,
            })
            .collect();
        assert_eq!(
            positions,
            vec![
                LogicalPoint::new(px(1.0), px(2.0)),
                LogicalPoint::new(px(9.0), px(2.0))
            ]
        );
    }

    #[test]
    fn without_stale_glyph_runs_drops_evicted_tiles_and_keeps_live_ones() {
        let mut atlas = crate::render::AtlasAllocator::with_page(AtlasKind::Glyph, (64, 64));
        let kept = atlas.allocate((8, 8)).expect("fits");
        let evicted = atlas.allocate((8, 8)).expect("fits");
        let run_with = |tile: AtlasTile| GlyphRun {
            color: Color::BLACK,
            glyphs: vec![PositionedGlyph {
                tile,
                position: LogicalPoint::new(px(1.0), px(2.0)),
                size: LogicalSize::new(px(8.0), px(8.0)),
            }],
        };
        let mut canvas = Canvas::new(bounds());
        assert!(canvas.push_clip(clip()));
        canvas.push_text(run_with(evicted.tile));
        canvas.push_text(run_with(kept.tile));
        assert!(canvas.pop_clip());
        canvas.push_quad(quad());

        assert!(
            canvas.without_stale_glyph_runs(&atlas).is_none(),
            "无失效 tile 时不得产生任何过滤（回归：正常路径零额外过滤）"
        );

        let generation = atlas.generation();
        atlas.deallocate(evicted.id);
        let replacement = atlas.allocate((8, 8)).expect("淘汰出的槽位会被复用");
        assert_eq!(
            replacement.tile.slot, evicted.tile.slot,
            "本页只放得下两块：淘汰出的槽位必然被复用"
        );
        assert_ne!(
            replacement.tile.slot_epoch, evicted.tile.slot_epoch,
            "槽位复用必须换纪元"
        );
        assert_eq!(
            atlas.generation(),
            generation,
            "F15 的关键前提：逐 tile 淘汰不换代"
        );

        let (filtered, dropped) = canvas
            .without_stale_glyph_runs(&atlas)
            .expect("失效 tile 的 run 必须被过滤");
        assert_eq!(dropped, 1, "只丢弃失效的那一条 run（诊断计数 +1）");
        let kinds: Vec<(u8, bool)> = filtered
            .primitives()
            .iter()
            .map(|primitive| match primitive {
                Primitive::Clip(ClipOp::Push(_)) => (0, false),
                Primitive::GlyphRun(run) => (1, run.glyphs[0].tile == kept.tile),
                Primitive::Clip(ClipOp::Pop) => (2, false),
                Primitive::Quad(_) => (3, false),
                _ => (9, false),
            })
            .collect();
        assert_eq!(
            kinds,
            vec![(0, false), (1, true), (2, false), (3, false)],
            "过滤后顺序 = 原顺序，clip 括号与非字形图元保留，只剩有效 tile 的 run"
        );
        assert_eq!(filtered.bounds(), canvas.bounds(), "bounds 不变");
    }
}
