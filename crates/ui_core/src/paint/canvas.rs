//! 应用层唯一公开写入口：`Canvas`。
//!
//! `Canvas` 内部持有一个 crate 私有的绘制桶（`scene::Lane`），公开的 `push_*` 方法把引擎图元写进去；
//! 引擎在 P03/P07 把它包成 `Primitive::Canvas` 嵌进真实 lane。所有权与 API 契约见
//! `design/phase-02-paint-foundation.md` §11。

use crate::{
    math::Rect,
    paint::{
        atlas::AtlasKind,
        primitive::{
            ClipOp, ClipRect, GeometryDraw, GlyphRun, Image, Path, Primitive, Quad, Shadow,
        },
        scene::{Lane, MAX_CLIP_DEPTH},
    },
};

/// §7.4 C4：clip 矩形的坐标/尺寸/圆角都必须是有限值，尺寸非负。
fn is_well_formed(clip: &ClipRect) -> bool {
    let rect = clip.rect;
    rect.origin.x.is_finite()
        && rect.origin.y.is_finite()
        && rect.size.width.is_finite()
        && rect.size.height.is_finite()
        && rect.size.width >= 0.0
        && rect.size.height >= 0.0
        && clip.corner_radius.iter().all(|radius| radius.is_finite())
}

/// 一组引擎图元的记录容器 + 应用层写入口（值语义：应用构造 → 交给引擎）。
///
/// 尺寸 48 B（`Rect` 16 + `Lane` 32）。
#[derive(Clone, Debug, PartialEq)]
pub struct Canvas {
    /// 逻辑像素 AABB。契约：必须 ⊇ 所有内部图元的 bounds 并集（damage / 命中只用它）。
    bounds: Rect,
    lane: Lane,
}

impl Canvas {
    /// 应用侧的构造入口（初始为空 lane）。
    ///
    /// # Panics: none
    #[must_use]
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            lane: Lane::new(),
        }
    }

    /// # Panics: none
    pub fn push_quad(&mut self, quad: Quad) {
        self.lane.push(Primitive::Quad(quad));
    }

    /// 已塑形的文本。**应用层不应自行构造 [`GlyphRun`]**：其中的 `AtlasTile` 只能由图集分配器
    /// （`render/atlas.rs`，P05）产出，唯一合法调用者是 P06 的文本 painter；应用层要文本请用
    /// `NodeBuilder::text()`（`design/phase-02-paint-foundation.md` §11.5）。
    ///
    /// debug 构建断言每个字形的 `tile.atlas == AtlasKind::Glyph`（放错图集 = 采样到图片页）。
    ///
    /// # Panics: none
    pub fn push_text(&mut self, run: GlyphRun) {
        debug_assert!(
            run.glyphs
                .iter()
                .all(|glyph| glyph.tile.atlas == AtlasKind::Glyph),
            "glyph run contains a tile from a non-glyph atlas"
        );
        self.lane.push(Primitive::GlyphRun(run));
    }

    /// debug 构建断言 `image.tile.atlas == AtlasKind::Image`。
    ///
    /// # Panics: none
    pub fn push_image(&mut self, image: Image) {
        debug_assert!(
            image.tile.atlas == AtlasKind::Image,
            "image tile must come from the image atlas"
        );
        self.lane.push(Primitive::Image(image));
    }

    /// # Panics: none
    pub fn push_path(&mut self, path: Path) {
        self.lane.push(Primitive::Path(path));
    }

    /// # Panics: none
    pub fn push_shadow(&mut self, shadow: Shadow) {
        self.lane.push(Primitive::Shadow(shadow));
    }

    /// # Panics: none
    pub fn push_geometry(&mut self, draw: GeometryDraw) {
        self.lane.push(Primitive::Geometry(draw));
    }

    /// 压栈 clip：深度未超 `MAX_CLIP_DEPTH`（8，crate 私有常量）时压入并返回 `true`，否则**不写入**并返回 `false`。
    ///
    /// debug 构建断言 `clip.rect` 的尺寸非负且坐标/圆角有限（§7.4 C4）；release 构建不做任何检查。
    ///
    /// # Panics: none
    #[must_use]
    pub fn push_clip(&mut self, clip: ClipRect) -> bool {
        debug_assert!(
            is_well_formed(&clip),
            "clip rect must be finite with non-negative size"
        );
        if usize::from(self.lane.clip_depth()) >= usize::from(MAX_CLIP_DEPTH) {
            return false;
        }
        self.lane.push(Primitive::Clip(ClipOp::Push(clip)));
        true
    }

    /// 弹栈：深度 `> 0` 时弹出并返回 `true`，否则返回 `false`。
    ///
    /// # Panics: none
    #[must_use]
    pub fn pop_clip(&mut self) -> bool {
        if self.lane.clip_depth() == 0 {
            return false;
        }
        self.lane.push(Primitive::Clip(ClipOp::Pop));
        true
    }

    /// # Panics: none
    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// # Panics: none
    #[must_use]
    pub fn primitives(&self) -> &[Primitive] {
        self.lane.primitives()
    }

    /// # Panics: none
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lane.is_empty()
    }
}

impl Default for Canvas {
    /// `bounds` 为零矩形、空 lane；与 `Canvas::new(Rect::default())` 等价。
    fn default() -> Self {
        Self::new(Rect::default())
    }
}

#[cfg(test)]
mod tests {
    use super::Canvas;
    use crate::{
        math::{Point, Rect, Size},
        paint::{
            atlas::{AtlasKind, AtlasTile},
            color::Color,
            primitive::{
                ClipRect, GlyphRun, Image, PositionedGlyph, Primitive, Quad, QuadPaint, Shadow,
            },
            scene::MAX_CLIP_DEPTH,
        },
    };

    fn bounds() -> Rect {
        Rect::new(0.0, 0.0, 64.0, 32.0)
    }

    fn quad() -> Quad {
        Quad {
            bounds: bounds(),
            corner_radius: [0.0; 4],
            paint: QuadPaint::Solid {
                fill: Color::WHITE,
                border: None,
            },
        }
    }

    fn clip() -> ClipRect {
        ClipRect {
            rect: bounds(),
            corner_radius: [0.0; 4],
        }
    }

    fn image() -> Image {
        Image {
            bounds: bounds(),
            tile: AtlasTile {
                atlas: AtlasKind::Image,
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 1.0,
                generation: 0,
            },
            modulation: Color::WHITE,
        }
    }

    fn shadow() -> Shadow {
        Shadow {
            rect: bounds(),
            offset: Point::new(0.0, 1.0),
            blur: 2.0,
            color: Color::BLACK,
        }
    }

    fn glyph_run() -> GlyphRun {
        GlyphRun {
            color: Color::BLACK,
            glyphs: vec![PositionedGlyph {
                tile: AtlasTile {
                    atlas: AtlasKind::Glyph,
                    u0: 0.0,
                    v0: 0.0,
                    u1: 0.5,
                    v1: 0.5,
                    generation: 0,
                },
                position: Point::new(2.0, 3.0),
                size: Size::new(8.0, 8.0),
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
        assert_eq!(canvas, Canvas::new(Rect::default()));
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
            assert!(canvas.push_clip(clip()));
        }
        let before = canvas.primitives().len();
        assert!(!canvas.push_clip(clip()));
        assert_eq!(canvas.primitives().len(), before);
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
        // Rect(16) + Lane(32)：Lane 增大就会抬高每个自绘节点的成本（§2.5）。
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
        clip.rect = Rect::new(0.0, 0.0, -1.0, 8.0);
        let _ = canvas.push_clip(clip);
    }

    #[test]
    fn canvas_is_clone_and_eq() {
        let mut canvas = Canvas::new(bounds());
        canvas.push_quad(quad());
        let clone = canvas.clone();
        assert_eq!(clone, canvas);
    }
}
