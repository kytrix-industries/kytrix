//! 图集分配器（P02 §10.4 的落地）：单页 + 1 纹素 padding + 脏行 + 代际。
//!
//! 设计见 `design/phase-05-text.md` §9（逐字规格）与 `design/phase-02-paint-foundation.md` §10。
//! 只做**装箱 + 脏行 + 代际**：不持有 CPU 像素（P02 §10.6），上传由 P09 从字形缓存读。
//!
//! 两条 probe 实测修正（phase-05 §9.2）：
//! 1. `etagere` 会把请求矩形**向上取整**（请求 8×11 得 8×16）⇒ `padded` 取 `Allocation::rectangle` 的实际值；
//! 2. 陈旧 `AllocId` 的 `deallocate` 会 `assert!` panic ⇒ [`AtlasAllocator::deallocate`] 必须自己判重。

use std::ops::Range;

use etagere::{AllocId, AtlasAllocator as EtagereAllocator, size2};

use crate::{
    math::PhysicalRect,
    paint::atlas::{AtlasKind, AtlasTile},
};

/// 字形页尺寸（纹素）：2048² R8 = 4 MiB（P02 §10.3）。
pub(crate) const GLYPH_PAGE: (u32, u32) = (2048, 2048);
/// 图片页尺寸（纹素）：2048² BGRA8 = 16 MiB（P02 §10.3）。
pub(crate) const IMAGE_PAGE: (u32, u32) = (2048, 2048);

/// 一个 `AtlasKind` 的**单页**分配器（P02 §10.4）。
pub(crate) struct AtlasAllocator {
    kind: AtlasKind,
    page: (u32, u32),
    allocator: EtagereAllocator,
    /// 槽位表（仅测试构建：v1 没有逐 tile 逐出，生产路径只靠 `clear()` 换代）。
    #[cfg(test)]
    tiles: Vec<Option<AllocId>>,
    #[cfg(test)]
    free: Vec<usize>,
    generation: u32,
    dirty: DirtyRows,
}

/// 一次分配的结果。`id` 是**唯一**可交给 [`AtlasAllocator::deallocate`] 的键。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Allocation {
    /// 释放键（etagere 对非法 id 会 panic，见模块文档）。
    pub id: AllocId,
    /// 可直接进 `Primitive::GlyphRun` / `Primitive::Image` 的图集矩形（内容区 UV + 代际）。
    pub tile: AtlasTile,
    /// 内容区像素矩形（页面坐标，不含 padding）。
    pub content: PhysicalRect,
    /// 实际分配到的矩形（**可能**被 etagere 向上取整放大，含 padding）。
    pub padded: PhysicalRect,
}

impl AtlasAllocator {
    /// 新建（页尺寸按 `kind` 的内置常量）。
    pub(crate) fn new(kind: AtlasKind) -> Self {
        Self::with_page(kind, page_of(kind))
    }

    fn with_page(kind: AtlasKind, page: (u32, u32)) -> Self {
        Self {
            kind,
            page,
            allocator: EtagereAllocator::new(size2(page.0 as i32, page.1 as i32)),
            #[cfg(test)]
            tiles: Vec::with_capacity(64),
            #[cfg(test)]
            free: Vec::new(),
            generation: 1,
            dirty: DirtyRows::default(),
        }
    }

    /// 仅测试：用小页构造（2048² 填满需要几十万次分配）。
    #[cfg(test)]
    pub(crate) fn for_test(kind: AtlasKind, page: (u32, u32)) -> Self {
        Self::with_page(kind, page)
    }

    pub(crate) fn kind(&self) -> AtlasKind {
        self.kind
    }

    pub(crate) fn page(&self) -> (u32, u32) {
        self.page
    }

    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }

    /// 分配 `content` 大小的内容区（内部加 1 纹素 padding）。`None` = 本页放不下。
    pub(crate) fn allocate(&mut self, content: (u32, u32)) -> Option<Allocation> {
        let (width, height) = content;
        // 1. 拒绝空尺寸
        if width == 0 || height == 0 {
            return None;
        }
        // 2+3. 拒绝超出 etagere 的 u16 寻址 / 单列页宽
        let padded = (width.checked_add(2)?, height.checked_add(2)?);
        if padded.0 > u32::from(u16::MAX) || padded.1 > u32::from(u16::MAX) {
            return None;
        }
        if padded.0 > self.page().0 || padded.1 > self.page().1 {
            return None;
        }
        // 5. etagere 装箱（返回矩形可能更大，见模块文档）
        let allocation = self
            .allocator
            .allocate(size2(padded.0 as i32, padded.1 as i32))?;
        let rectangle = allocation.rectangle;
        // 6. 内容区原点 = 分配矩形左上 + 1 纹素 padding
        let origin_x = rectangle.min.x + 1;
        let origin_y = rectangle.min.y + 1;
        // 7. UV 用边、不用纹素中心（P02 §10.2）
        let page_w = self.page.0 as f32;
        let page_h = self.page.1 as f32;
        let tile = AtlasTile {
            atlas: self.kind(),
            u0: origin_x as f32 / page_w,
            v0: origin_y as f32 / page_h,
            u1: (origin_x + width as i32) as f32 / page_w,
            v1: (origin_y + height as i32) as f32 / page_h,
            generation: self.generation,
        };
        let content_rect = PhysicalRect::new(
            origin_x,
            origin_y,
            origin_x + width as i32,
            origin_y + height as i32,
        );
        let padded_rect = PhysicalRect::new(
            rectangle.min.x,
            rectangle.min.y,
            rectangle.max.x,
            rectangle.max.y,
        );
        debug_assert_eq!(tile.atlas, self.kind(), "tile must carry its atlas kind");
        #[cfg(test)]
        {
            if let Some(index) = self.free.pop() {
                self.tiles[index] = Some(allocation.id);
            } else {
                self.tiles.push(Some(allocation.id));
            }
        }
        self.mark_written(content_rect);
        Some(Allocation {
            id: allocation.id,
            tile,
            content: content_rect,
            padded: padded_rect,
        })
    }

    /// 幂等释放：`id` 不在槽位表里就忽略（etagere 对陈旧 id 会 panic）。
    #[cfg(test)]
    pub(crate) fn deallocate(&mut self, id: AllocId) {
        let Some(index) = self
            .tiles
            .iter()
            .position(|slot| matches!(slot, Some(existing) if *existing == id))
        else {
            return;
        };
        self.tiles[index] = None;
        self.free.push(index);
        self.allocator.deallocate(id);
    }

    /// 清空并 `generation += 1`（DPI 变化 / 字体换代 / 容量逃逸）。
    pub(crate) fn clear(&mut self) {
        self.allocator = EtagereAllocator::new(size2(self.page.0 as i32, self.page.1 as i32));
        #[cfg(test)]
        {
            self.tiles.clear();
            self.free.clear();
        }
        self.dirty.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// 记录「这些行被写脏了」（P02 §10.5）。
    pub(crate) fn mark_written(&mut self, rect: PhysicalRect) {
        if rect.is_empty() {
            return;
        }
        self.dirty
            .mark(rect.y0.max(0) as u32, rect.y1.max(0) as u32);
    }

    /// 取走脏行（P09 的增量上传目标）。`App::layout_text` 把它合并进 `App::pending_dirty`。
    pub(crate) fn take_dirty_rows(&mut self) -> DirtyRows {
        core::mem::take(&mut self.dirty)
    }
}

const fn page_of(kind: AtlasKind) -> (u32, u32) {
    match kind {
        AtlasKind::Glyph => GLYPH_PAGE,
        AtlasKind::Image => IMAGE_PAGE,
    }
}

/// 两类图集的集合（P02 §5.6：字形与图片分图集；v1 每类单页）。
pub(crate) struct AtlasSet {
    glyph: AtlasAllocator,
    image: AtlasAllocator,
}

impl AtlasSet {
    pub(crate) fn new() -> Self {
        Self {
            glyph: AtlasAllocator::new(AtlasKind::Glyph),
            image: AtlasAllocator::new(AtlasKind::Image),
        }
    }

    pub(crate) fn glyph(&self) -> &AtlasAllocator {
        &self.glyph
    }

    pub(crate) fn glyph_mut(&mut self) -> &mut AtlasAllocator {
        &mut self.glyph
    }

    /// 图片页（P15 的写入目标）。
    pub(crate) fn image(&mut self) -> &mut AtlasAllocator {
        &mut self.image
    }

    /// 两页都换代（DPI 变化 / 字体切换 / 容量逃逸）。
    pub(crate) fn clear_all(&mut self) {
        self.glyph.clear();
        self.image().clear();
    }

    /// 仅测试：图片页尺寸。
    #[cfg(test)]
    pub(crate) fn image_page_for_test(&self) -> (u32, u32) {
        self.image.page()
    }

    /// 仅测试：图片页代际。
    #[cfg(test)]
    pub(crate) fn image_generation_for_test(&self) -> u32 {
        self.image.generation()
    }
}

impl Default for AtlasSet {
    fn default() -> Self {
        Self::new()
    }
}

/// 被写脏的**行区间**集合（y 区间，x 覆盖整页），升序、已合并。
#[derive(Clone, Debug, Default)]
pub(crate) struct DirtyRows {
    spans: Vec<Range<u32>>,
}

impl DirtyRows {
    /// 标记 `[y0, y1)` 脏；相邻/重叠都合并；非法区间忽略；段数 > 8 折叠为整页（0..u32::MAX）。
    pub(crate) fn mark(&mut self, y0: u32, y1: u32) {
        if y0 >= y1 || self.is_full() {
            return;
        }
        let mut start = y0;
        let mut end = y1;
        let mut out: Vec<Range<u32>> = Vec::with_capacity(self.spans.len() + 1);
        for span in self.spans.drain(..) {
            if span.end < start || span.start > end {
                out.push(span);
            } else {
                start = start.min(span.start);
                end = end.max(span.end);
            }
        }
        out.push(start..end);
        out.sort_by_key(|span| span.start);
        if out.len() > 8 {
            self.spans.clear();
            self.spans.push(0..u32::MAX);
        } else {
            self.spans = out;
        }
    }

    pub(crate) fn spans(&self) -> &[Range<u32>] {
        &self.spans
    }

    /// 是否退化为「整页」。
    pub(crate) fn is_full(&self) -> bool {
        self.spans
            .iter()
            .any(|span| span.start == 0 && span.end == u32::MAX)
    }

    pub(crate) fn clear(&mut self) {
        self.spans.clear();
    }

    /// 合并另一份脏行（P05 把每轮 `take_dirty_rows` 的结果并入 `App::pending_dirty`）。
    pub(crate) fn merge(&mut self, other: &mut DirtyRows) {
        for span in other.spans.drain(..) {
            self.mark(span.start, span.end);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AtlasAllocator, AtlasSet, DirtyRows, GLYPH_PAGE};
    use crate::paint::atlas::AtlasKind;

    fn allocator() -> AtlasAllocator {
        AtlasAllocator::for_test(AtlasKind::Glyph, (64, 64))
    }

    #[test]
    fn allocate_adds_one_texel_padding() {
        let mut atlas = allocator();
        let allocation = atlas.allocate((6, 9)).expect("6x9 must fit");

        assert_eq!(allocation.content.width(), 6);
        assert_eq!(allocation.content.height(), 9);
        assert!(
            allocation.padded.width() >= 8,
            "padded width {} must cover content + 2",
            allocation.padded.width()
        );
        assert!(
            allocation.padded.height() >= 11,
            "padded height {} must cover content + 2",
            allocation.padded.height()
        );
        assert_eq!(
            allocation.content.x0,
            allocation.padded.x0 + 1,
            "content starts one texel inside the padded rect"
        );
        assert_eq!(allocation.content.y0, allocation.padded.y0 + 1);
    }

    #[test]
    fn uv_uses_edges_and_stays_inside_the_page() {
        let mut atlas = AtlasAllocator::for_test(AtlasKind::Glyph, (64, 64));
        let allocation = atlas.allocate((6, 9)).expect("fits");
        let tile = allocation.tile;

        assert!((tile.u1 - tile.u0 - 6.0 / 64.0).abs() < 1e-6);
        assert!((tile.v1 - tile.v0 - 9.0 / 64.0).abs() < 1e-6);
        assert!(tile.u0 >= 0.0 && tile.u1 <= 1.0);
        assert!(tile.v0 >= 0.0 && tile.v1 <= 1.0);
        let texel_x = tile.u0 * 64.0;
        assert!(
            (texel_x - texel_x.round()).abs() < 1e-4,
            "u0 must land on a texel boundary, got {texel_x}"
        );
    }

    #[test]
    fn default_page_sizes_match_paint_contract() {
        assert_eq!(GLYPH_PAGE, (2048, 2048));
        let set = AtlasSet::new();
        assert_eq!(set.glyph().page(), (2048, 2048));
        assert_eq!(set.glyph().kind(), AtlasKind::Glyph);
        assert_eq!(set.image_page_for_test(), (2048, 2048));
    }

    #[test]
    fn deallocate_is_idempotent_and_ignores_unknown_ids() {
        let mut atlas = allocator();
        let allocation = atlas.allocate((8, 8)).expect("fits");
        let reused = atlas.allocate((8, 8)).expect("fits");

        atlas.deallocate(allocation.id);
        atlas.deallocate(allocation.id); // 幂等：etagere 对陈旧 id 会 panic
        atlas.deallocate(reused.id);
        assert!(atlas.allocate((8, 8)).is_some(), "freed slots are reusable");
    }

    #[test]
    fn release_restores_capacity() {
        let mut atlas = AtlasAllocator::for_test(AtlasKind::Glyph, (16, 16));
        let first = atlas.allocate((12, 12)).expect("fits");
        assert!(
            atlas.allocate((12, 12)).is_none(),
            "a 16x16 page holds only one 14x14 padded tile"
        );
        atlas.deallocate(first.id);
        assert!(atlas.allocate((12, 12)).is_some());
    }

    #[test]
    fn clear_bumps_generation_and_makes_tiles_stale() {
        let mut atlas = allocator();
        let allocation = atlas.allocate((4, 4)).expect("fits");
        let before = atlas.generation();
        let old_tile = allocation.tile;

        atlas.clear();

        assert_eq!(atlas.generation(), before + 1);
        assert!(old_tile.is_stale(atlas.generation()));
        assert!(!old_tile.is_stale(before));
    }

    #[test]
    fn zero_and_over_page_requests_are_rejected() {
        let mut atlas = allocator();

        assert!(atlas.allocate((0, 10)).is_none());
        assert!(atlas.allocate((10, 0)).is_none());
        assert!(atlas.allocate((64, 64)).is_none(), "needs 66 > page width");
        assert!(atlas.allocate((300, 10)).is_none());
    }

    #[test]
    fn exhaustion_then_clear_retry_succeeds() {
        let mut atlas = AtlasAllocator::for_test(AtlasKind::Glyph, (32, 32));
        let mut allocated = 0usize;
        while atlas.allocate((6, 6)).is_some() {
            allocated += 1;
            assert!(allocated < 4096, "must terminate");
        }
        assert!(allocated > 0);
        assert!(atlas.allocate((6, 6)).is_none());

        atlas.clear();
        assert!(
            atlas.allocate((6, 6)).is_some(),
            "after clear the same request must fit"
        );
    }

    #[test]
    fn dirty_rows_merge_adjacent_and_fold_to_full() {
        let mut rows = DirtyRows::default();
        rows.mark(10, 20);
        rows.mark(20, 30);
        rows.mark(0, 5);
        assert_eq!(rows.spans(), &[0..5, 10..30]);
        assert!(!rows.is_full());

        for index in 0..12u32 {
            rows.mark(index * 8, index * 8 + 1);
        }
        assert!(rows.is_full(), "more than 8 spans folds to the whole page");
        rows.clear();
        assert!(rows.spans().is_empty());
    }

    #[test]
    fn dirty_rows_ignore_invalid_ranges() {
        let mut rows = DirtyRows::default();
        rows.mark(10, 10);
        rows.mark(20, 5);
        assert!(rows.spans().is_empty());
    }

    #[test]
    fn mark_written_feeds_dirty_rows_and_take_drains_them() {
        let mut atlas = allocator();
        let _ = atlas.allocate((5, 7)).expect("fits");
        let rows = atlas.take_dirty_rows();
        assert_eq!(rows.spans().len(), 1);
        assert!(atlas.take_dirty_rows().spans().is_empty(), "take drains");
    }

    #[test]
    fn atlas_set_keeps_kinds_independent_and_clears_together() {
        let mut set = AtlasSet::new();
        let glyph_before = set.glyph_mut().generation();
        let image_before = set.image_generation_for_test();
        let glyph_tile = set.glyph_mut().allocate((8, 8)).expect("fits").tile;

        set.clear_all();

        assert_eq!(set.glyph_mut().generation(), glyph_before + 1);
        assert_eq!(set.image_generation_for_test(), image_before + 1);
        assert!(glyph_tile.is_stale(set.glyph_mut().generation()));
    }
}
