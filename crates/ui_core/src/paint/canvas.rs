//! 应用层唯一公开写入口：`Canvas`。
//!
//! `Canvas` 内部持有一个 crate 私有的绘制桶（`scene::Lane`），公开的 `push_*` 方法把引擎图元写进去；
//! 引擎在 P03/P07 把它包成 `Primitive::Canvas` 嵌进真实 lane。所有权与 API 契约见

use std::sync::Arc;

use crate::{
    math::{Point, Rect},
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

    /// 应用层画文本的**唯一**合法入口（v2.1 E1，`§11.1`）：按 run 拆 `Primitive::GlyphRun`，
    /// 每个字形的落点平移 `origin`。
    ///
    /// `origin` 是文本盒左上角的**绝对窗口逻辑像素**（与 `Canvas` 内其它图元同一坐标系，
    /// §5.9）；多行文本的所有行一起偏移。字形来自 [`ShapedTextHandle`]，应用不接触
    /// `AtlasTile` / `GlyphRun` 的构造。
    ///
    /// **引擎负责租约校验**（等价 D-5 的「唯一重光栅入口」）：句柄在图集换代**或**其 tile
    /// 被逐 tile 淘汰后已失效时**不产生任何图元**并返回 `false` —— 绝不把旧 UV 编码进图元
    /// （那正是 D-5 修掉的花字）。应用收到 `false` 应丢弃句柄并重新 `App::shape_text`。
    ///
    /// push **之后**才发生的图集换代**或逐 tile LRU 淘汰**（不换代）由 paint 的编码期安全网
    /// 兜底（[`Canvas::without_stale_glyph_runs`]：逐 tile 判活，失效 run 不编码）；但 Canvas
    /// 内容归应用所有，应用仍须在句柄失效后重建 Canvas（`§11.1` 的应用义务）。
    ///
    /// 空文本（0 行）返回 `true` 且不产生图元：句柄本身有效，只是没有可画内容。
    ///
    /// # Panics: none
    #[must_use]
    pub fn push_shaped_text(&mut self, handle: &ShapedTextHandle, origin: Point) -> bool {
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
                        position: Point::new(
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

    /// 编码期安全网（v2.1 E1b + E1d，`§11.1` 的「不允许画旧 UV」）：按图集**当前状态**过滤
    /// Canvas 内的字形 run。
    ///
    /// [`Canvas::push_shaped_text`] 只在 **push 时**校验句柄租约；push 之后发生的图集换代
    /// （`layout_text` 扩容 / 清空、换字体栈）**或逐 tile LRU 淘汰**（不换代）都会让已写入的
    /// `AtlasTile` 指向旧 UV。paint 在 emit 本 Canvas 前调用本方法：逐 tile 用
    /// [`AtlasAllocator::is_alive`]（代际 + 槽位分配身份 `(slot, slot_epoch)`，O(1)）校验，
    /// 任一 tile 已失效的 run **整段丢弃**（少字优于花字，与节点文本的 D-5 编码期硬校验同口径），
    /// 其余图元（含 clip 括号）原样保留、顺序不变。
    ///
    /// 返回 `None` = 没有失效 run（调用方复用原值，零分配）；否则返回过滤后的 Canvas 与
    /// **丢弃的 run 数**（`PaintReport::stale_canvas_runs` 的诊断计数）。
    ///
    /// # Panics: none
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
                    // 逐 tile 判活：整页换代（generation 前进）与逐 tile 淘汰（槽位纪元前进）
                    // 都由 `is_alive` 覆盖，且都是 O(1)。
                    if run.glyphs.iter().any(|glyph| !atlas.is_alive(&glyph.tile)) {
                        dropped += 1;
                        changed = true;
                    } else {
                        filtered.push(primitive.clone());
                    }
                }
                // 嵌套 Canvas（应用构造不出口，但值语义下可能经其它路径出现）：递归过滤。
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
                ClipOp, ClipRect, GlyphRun, Image, PositionedGlyph, Primitive, Quad, QuadPaint,
                Shadow,
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
                slot: 0,
                slot_epoch: 0,
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
                    slot: 0,
                    slot_epoch: 0,
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

    /// v2.1 E1b：`push_shaped_text` 的唯一入口语义 —— 图集租约校验 + origin 偏移 + 按 run 拆图元。
    ///
    /// `(generation, eviction_epoch)` 是句柄构造时的租约，`current` 是共享租约里的当前值：
    /// 任一不等 ⇒ 句柄已失效（整页换代与逐 tile 淘汰都算）。
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
            position: Point::new(2.0, 3.0),
            size: Size::new(8.0, 8.0),
        };
        let line = ShapedLine {
            text: smol_str::SmolStr::new_static("e1"),
            lines: vec![ShapedTextLine {
                text_range: 0..2,
                baseline: 12.0,
                line_height: 14.0,
                advance: 10.0,
                items: vec![ShapedTextItem::GlyphRun(ShapedGlyphRun {
                    color: Color::BLACK,
                    text_range: 0..2,
                    glyphs: vec![glyph],
                })],
            }],
            width: 10.0,
            height: 14.0,
            first_baseline: Some(12.0),
            atlas_generation: generation,
            font_stack_version: 1,
            scale: 1.0,
        };
        ShapedTextHandle::new(
            Arc::new(line),
            Box::new([]),
            generation,
            eviction_epoch,
            1.0,
            Arc::new(AtomicU64::new(pack_atlas_lease(current.0, current.1))),
        )
    }

    #[test]
    fn push_shaped_text_offsets_glyphs_by_the_origin() {
        let mut canvas = Canvas::new(bounds());
        let handle = shaped_handle(7, 2, (7, 2));
        assert!(
            canvas.push_shaped_text(&handle, Point::new(10.0, 20.0)),
            "当前代际的句柄必须能编码"
        );
        assert_eq!(canvas.primitives().len(), 1);
        let Primitive::GlyphRun(run) = &canvas.primitives()[0] else {
            panic!("push_shaped_text 只能产生 GlyphRun 图元");
        };
        let glyph = run.glyphs[0];
        assert_eq!(
            glyph.position,
            Point::new(10.0 + 2.0, 20.0 + 3.0),
            "origin 是文本盒左上角的绝对窗口逻辑像素"
        );
        assert_eq!(glyph.size, Size::new(8.0, 8.0));
        assert_eq!(glyph.tile.generation, 7);
        assert_eq!(run.color, Color::BLACK);
    }

    #[test]
    fn push_shaped_text_refuses_a_stale_handle() {
        let mut canvas = Canvas::new(bounds());
        canvas.push_quad(quad());
        let before = canvas.primitives().len();
        // 句柄记录 7 代，图集已经到 8 代 ⇒ 旧 UV 已失效。
        let stale = shaped_handle(7, 0, (8, 0));
        assert!(stale.is_stale());
        assert!(
            !canvas.push_shaped_text(&stale, Point::ZERO),
            "stale 句柄必须返回 false"
        );
        assert_eq!(
            canvas.primitives().len(),
            before,
            "stale push 不得产生任何图元（不画旧 UV）"
        );
        // E1b F1：代际不变、但句柄构造后发生过逐 tile 淘汰 ⇒ 同样失效。
        let evicted = shaped_handle(7, 2, (7, 3));
        assert!(
            evicted.is_stale(),
            "淘汰纪元前进必须让句柄失效（tile 已被别的字形复用）"
        );
        assert!(
            !canvas.push_shaped_text(&evicted, Point::ZERO),
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
                position: Point::new(x, 0.0),
                size: Size::new(8.0, 8.0),
            }],
        };
        let line = ShapedLine {
            text: smol_str::SmolStr::new_static("ab"),
            lines: vec![ShapedTextLine {
                text_range: 0..2,
                baseline: 12.0,
                line_height: 14.0,
                advance: 16.0,
                items: vec![
                    ShapedTextItem::GlyphRun(run(0.0)),
                    ShapedTextItem::InlineBox(crate::text::ShapedInlineBox {
                        id: 5,
                        rect: Rect::new(4.0, 0.0, 8.0, 8.0),
                        baseline: None,
                    }),
                    ShapedTextItem::GlyphRun(run(8.0)),
                ],
            }],
            width: 16.0,
            height: 14.0,
            first_baseline: Some(12.0),
            atlas_generation: 3,
            font_stack_version: 1,
            scale: 1.0,
        };
        let handle = ShapedTextHandle::new(
            Arc::new(line),
            Box::new([]),
            3,
            0,
            1.0,
            Arc::new(AtomicU64::new(pack_atlas_lease(3, 0))),
        );
        let mut canvas = Canvas::new(bounds());
        assert!(canvas.push_shaped_text(&handle, Point::new(1.0, 2.0)));
        assert_eq!(
            canvas.primitives().len(),
            2,
            "按 run 拆图元：行内盒不产生图元（E1 只保证落点可读）"
        );
        let positions: Vec<Point> = canvas
            .primitives()
            .iter()
            .filter_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => run.glyphs.first().map(|glyph| glyph.position),
                _ => None,
            })
            .collect();
        assert_eq!(positions, vec![Point::new(1.0, 2.0), Point::new(9.0, 2.0)]);
    }

    /// E1d（F15 方案①）：编码期安全网按**分配身份**逐 tile 判活。
    ///
    /// 这里直接构造「已被复用槽位」的旧 tile（`generation` **不变**）：这正是 E1b 的代际过滤
    /// 无法检测、F15 记录的场景 —— 逐 tile LRU 淘汰后旧 `GlyphRun` 仍在 Canvas 值里。
    ///
    /// 变异守卫：`is_alive` 恒真（或过滤删掉 `is_alive`）⇒ 本用例的「丢弃 1 条 / 保持顺序」
    /// 断言变红。
    #[test]
    fn without_stale_glyph_runs_drops_evicted_tiles_and_keeps_live_ones() {
        let mut atlas = crate::render::AtlasAllocator::with_page(AtlasKind::Glyph, (64, 64));
        let kept = atlas.allocate((8, 8)).expect("fits");
        let evicted = atlas.allocate((8, 8)).expect("fits");
        let run_with = |tile: AtlasTile| GlyphRun {
            color: Color::BLACK,
            glyphs: vec![PositionedGlyph {
                tile,
                position: Point::new(1.0, 2.0),
                size: Size::new(8.0, 8.0),
            }],
        };
        let mut canvas = Canvas::new(bounds());
        assert!(canvas.push_clip(clip()));
        canvas.push_text(run_with(evicted.tile));
        canvas.push_text(run_with(kept.tile));
        assert!(canvas.pop_clip());
        canvas.push_quad(quad());

        // 5. 正常路径：全部 tile 活着 ⇒ 零过滤（调用方复用原值、零分配）。
        assert!(
            canvas.without_stale_glyph_runs(&atlas).is_none(),
            "无失效 tile 时不得产生任何过滤（回归：正常路径零额外过滤）"
        );

        // 1. 逐 tile 淘汰（generation 不变）：槽位被新分配复用、纪元前进 ⇒ 旧 tile 失效。
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
        // 2. 未淘汰 tile 的 run 正常保留；顺序与 clip 括号、非字形图元原样不变。
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
