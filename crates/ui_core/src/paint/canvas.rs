//! 应用层唯一公开写入口：`Canvas`。
//!
//! `Canvas` 内部持有一个 crate 私有的绘制桶（`scene::Lane`），公开的 `push_*` 方法把引擎图元写进去；
//! 引擎在 P03/P07 把它包成 `Primitive::Canvas` 嵌进真实 lane。所有权与 API 契约见

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
    /// `NodeBuilder::text()`。
    ///
    /// debug 构建断言每个字形来自 glyph 图集且层号为 0（RD-8：gl­yph 图集只有 1 层）。
    ///
    /// # Panics: debug 构建下 run 含非法 tile（放错图集 / 层号非 0）时断言失败
    pub fn push_text(&mut self, run: GlyphRun) {
        debug_assert!(
            run.glyphs
                .iter()
                .all(|glyph| glyph.tile.atlas == AtlasKind::Glyph && glyph.tile.page == 0),
            "glyph run contains a tile from a non-glyph atlas or non-zero layer"
        );
        self.lane.push(Primitive::GlyphRun(run));
    }

    /// debug 构建断言 `image.tile.atlas == AtlasKind::Image` 且页号 < `MAX_IMAGE_PAGES`（页号上限；
    /// texture array 的层数是运行时值，见 `render::atlas::AtlasArraySpec`）。
    ///
    /// # Panics: debug 构建下 tile 非法（放错图集 / 越界层号）时断言失败
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

    /// 压栈 clip：**本 Canvas 自身**的圆角 clip 层数未超 `MAX_CLIP_DEPTH`（8，crate 私有常量）
    /// 时压入并返回 `true`，否则**不写入**并返回 `false`。
    ///
    /// 门限与编码器的预算**同口径**：只数**圆角**层（全部轴对齐矩形 clip 在编码期求交合并成一条
    /// **像素精确 scissor**，不占预算，见 `paint/instance.rs` 的 `ClipState`）。因此「本 Canvas
    /// 8 层圆角 + 任意祖先矩形 clip」的链长是 9 = `MAX_CLIP_ITEMS`，仍然合法（旧实现按**全部**
    /// push 计数判 9 > 8 ⇒ 每帧丢帧，AR2 P1-⑥ / AR2b P2-N5）。
    ///
    /// **已知边界（文档化，不是实现缺陷）**：`Canvas` 在构造期是独立值，看不到祖先的圆角层数，
    /// 所以这个门限只覆盖**本 Canvas 的子树**；祖先圆角层 + 本 Canvas 圆角层之和超过 8 时，
    /// 编码期会返回 `EngineError::ClipDepthExceeded`（每帧丢帧，**不**静默少裁）。生产节点树
    /// 路径只产直角矩形 clip（`paint/pass.rs` 的内容 clip 恒 `corner_radius: [0.0; 4]`，
    /// 超 8 层折叠成交集），只有应用自己用 `Canvas` 压圆角 clip 才可能撞到。
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
        if self.lane.rounded_clip_depth() >= usize::from(MAX_CLIP_DEPTH) {
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

    /// 圆角 clip（`corner_radius` 非全 0 ⇒ 编码期各占一层、计入 `MAX_CLIP_DEPTH` 预算）。
    fn rounded_clip() -> ClipRect {
        ClipRect {
            rect: bounds(),
            corner_radius: [4.0; 4],
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
            corner_radius: [0.0; 4],
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
            assert!(canvas.push_clip(rounded_clip()));
        }
        let before = canvas.primitives().len();
        assert!(!canvas.push_clip(rounded_clip()));
        assert_eq!(canvas.primitives().len(), before);
        // 弹出一层后恢复。
        assert!(canvas.pop_clip());
        assert!(canvas.push_clip(rounded_clip()));
    }

    /// AR2b P2-N5：门限只数**圆角**层（与编码器预算同口径）。直角矩形 clip 在编码期求交合并成
    /// 一条像素精确 scissor，不占预算 —— 旧实现按**全部** push 计数，12 层矩形会在第 9 层开始
    /// 被静默丢弃（内容越界可见）。
    #[test]
    fn rect_clips_do_not_consume_the_rounded_budget() {
        let mut canvas = Canvas::new(bounds());
        for _ in 0..12 {
            assert!(canvas.push_clip(clip()), "直角矩形 clip 不占圆角预算");
        }
        // 圆角层此时仍是 0 ⇒ 8 层圆角全部可压。
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

    /// RD-8：图片 tile 的层号是 texture array 索引，越界层号在编码/采样时是未定义行为，
    /// 应用层写入口必须在 debug 构建直接拒绝。
    #[test]
    #[should_panic(expected = "image tile page")]
    fn push_image_rejects_out_of_range_atlas_page() {
        let mut canvas = Canvas::new(bounds());
        let mut out_of_range = image();
        out_of_range.tile.page = crate::paint::atlas::MAX_IMAGE_PAGES;
        canvas.push_image(out_of_range);
    }

    /// RD-8：glyph 图集只有 1 层（layer 恒 0），非 0 层号的 run 是契约破坏。
    #[test]
    #[should_panic(expected = "glyph run contains a tile")]
    fn push_text_rejects_non_zero_glyph_atlas_page() {
        let mut canvas = Canvas::new(bounds());
        let mut run = glyph_run();
        run.glyphs[0].tile.page = 1;
        canvas.push_text(run);
    }
}
