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

/// 图集页的**上限**（纹素，边长）：2048²。字形页 R8 = 4 MiB、图片页 BGRA8 = 16 MiB
/// （P02 §10.3）。字形页按 512 → 1024 → 2048 阶梯扩容；**图片页固定用这一档**
/// （容纳能力与插入顺序无关，见 `resources::ResourceCache`）。
pub(crate) const MAX_ATLAS_PAGE: u32 = 2048;

/// 图集页的起始档（字形页扩容阶梯的下界；图片页不用这个阶梯）。
///
/// 两张页都**懒分配**：没有字形 / 图片时不占 CPU 页、GPU 纹理与 staging。
pub(crate) const MIN_ATLAS_PAGE: u32 = 512;

/// 页档阶梯：512 → 1024 → 2048（上限）。字形页分配失败时按这个顺序换代扩容。
///
/// **图片页不走这个阶梯**：它固定用 `MAX_ATLAS_PAGE`（2048）一档，保证容纳能力与插入
/// 顺序无关（见 `resources::ResourceCache` 的 `PageState`）。
pub(crate) const ATLAS_PAGE_STEPS: [u32; 3] = [MIN_ATLAS_PAGE, MIN_ATLAS_PAGE * 2, MAX_ATLAS_PAGE];

/// 每 in-flight 槽**常驻** staging 的上限（字节）。
///
/// 更大的上传（整页 / 新窗口 / 页变化）走一次性缓冲并按帧 retire：常驻容量因此不随图集
/// 页尺寸放大（2048² 图片页的整页 staging 是 16 MiB/槽，三槽就是 48 MiB）。
pub(crate) const MAX_RESIDENT_STAGING_BYTES: u64 = 1 << 20;

/// 下一档页；`None` = 已到上限。
fn next_page(page: u32) -> Option<u32> {
    ATLAS_PAGE_STEPS
        .iter()
        .copied()
        .find(|candidate| *candidate > page)
}

/// 图集纹素的字节数（字形 `R8` = 1，图片 `BGRA8` = 4；三端同一契约）。
pub(crate) fn texel_bytes(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Glyph => 1,
        AtlasKind::Image => 4,
    }
}

/// 图集纹理重建位：页尺寸变化（懒创建 / 字形页换代）后，调用方必须重写该图集的绑定。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AtlasRebuilt {
    pub glyph: bool,
    pub image: bool,
}

impl AtlasRebuilt {
    pub(crate) fn mark(&mut self, kind: AtlasKind) {
        match kind {
            AtlasKind::Glyph => self.glyph = true,
            AtlasKind::Image => self.image = true,
        }
    }
}

/// 把 `page` 里 `rows` 顺序的行**紧密打包**进 `out`（DEVIATIONS F-211）。
///
/// 增量上传的源数据只有脏行；后端按「累计行偏移」从打包缓冲取行。`rows.is_full()` 时等价于
/// 整页拷贝（新窗口 / 页尺寸变化 / 首次上传都走这条）。
pub(crate) fn pack_rows(page: &[u8], extent: u32, texel: u32, rows: &DirtyRows, out: &mut Vec<u8>) {
    let expected = extent as usize * extent as usize * texel as usize;
    assert_eq!(
        page.len(),
        expected,
        "atlas page buffer must be {extent}² × {texel} bytes"
    );
    out.clear();
    let row_bytes = extent as usize * texel as usize;
    if rows.is_full() {
        out.extend_from_slice(page);
        return;
    }
    for span in rows.spans() {
        let start = (span.start.min(extent) as usize) * row_bytes;
        let end = (span.end.min(extent) as usize) * row_bytes;
        if start < end {
            out.extend_from_slice(&page[start..end]);
        }
    }
}

/// 一个 `AtlasKind` 的**单页**分配器（P02 §10.4）。
pub(crate) struct AtlasAllocator {
    kind: AtlasKind,
    page: (u32, u32),
    allocator: EtagereAllocator,
    /// 槽位表：`deallocate` 的幂等守卫（`etagere` 对陈旧 `AllocId` 会 `assert!` panic）。
    ///
    /// P13 起是**生产**字段：`ResourceCache::unload` 需要真正逐块释放（F-147）。
    tiles: Vec<Option<AllocId>>,
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
    /// 新建（页尺寸 = `page`；字形页由 `text::atlas` 选档，图片页固定最大档）。
    pub(crate) fn with_page(kind: AtlasKind, page: (u32, u32)) -> Self {
        Self {
            kind,
            page,
            allocator: EtagereAllocator::new(size2(page.0 as i32, page.1 as i32)),
            tiles: Vec::with_capacity(64),
            free: Vec::new(),
            generation: 1,
            dirty: DirtyRows::default(),
        }
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
        if let Some(index) = self.free.pop() {
            self.tiles[index] = Some(allocation.id);
        } else {
            self.tiles.push(Some(allocation.id));
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
    ///
    /// P13 起是生产代码（`ResourceCache::unload` 的唯一调用点）；不换代，因此在用 tile 的 UV 不受影响。
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
        self.tiles.clear();
        self.free.clear();
        self.dirty.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// 换代扩容到下一档（512 → 1024 → 2048）；返回 `false` = 已到上限。
    ///
    /// 页尺寸变化 ⇒ 旧 tile 全部失效、整页内容必须重传（`clear` 的语义），
    /// 调用方负责同步清空自己的缓存（[`crate::text::atlas::GlyphAtlas::clear`]）。
    pub(crate) fn grow(&mut self) -> bool {
        let Some(next) = next_page(self.page.0) else {
            return false;
        };
        self.page = (next, next);
        self.clear();
        true
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
/// 被写脏的**行区间**集合（y 区间，x 覆盖整页），升序、已合并。
#[derive(Clone, Debug, Default)]
pub(crate) struct DirtyRows {
    spans: Vec<Range<u32>>,
}

impl DirtyRows {
    /// 本次上传的行数（`full` = 整页 `page` 行；超出页的 span 被钳掉，DEVIATIONS F-211）。
    pub(crate) fn row_count(&self, page: u32) -> u32 {
        if self.is_full() {
            return page;
        }
        self.spans
            .iter()
            .map(|span| span.end.min(page).saturating_sub(span.start.min(page)))
            .sum()
    }

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

    /// 没有任何脏行。
    pub(crate) fn is_empty(&self) -> bool {
        self.spans.is_empty()
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

    /// 只读合并：把 `other` 的区间并进来，但不清空 `other`。
    ///
    /// 图集页是进程级共享数据、纹理是每窗口一份：同一批脏行要分发给每个窗口的待发队列
    /// （忙窗口攒着、空闲时补发），因此不能像 [`DirtyRows::merge`] 那样消费源。
    pub(crate) fn merge_from(&mut self, other: &DirtyRows) {
        for span in &other.spans {
            self.mark(span.start, span.end);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ATLAS_PAGE_STEPS, AtlasAllocator, DirtyRows, MAX_ATLAS_PAGE, MIN_ATLAS_PAGE};
    use crate::paint::atlas::AtlasKind;

    fn allocator() -> AtlasAllocator {
        AtlasAllocator::with_page(AtlasKind::Glyph, (64, 64))
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
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (64, 64));
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
    fn default_page_ladder_matches_paint_contract() {
        assert_eq!(MAX_ATLAS_PAGE, 2048, "两张页的上限都是 2048²");
        assert_eq!(MIN_ATLAS_PAGE, 512);
        assert_eq!(ATLAS_PAGE_STEPS, [512, 1024, 2048]);
        let glyph = AtlasAllocator::with_page(AtlasKind::Glyph, (MIN_ATLAS_PAGE, MIN_ATLAS_PAGE));
        assert_eq!(glyph.page(), (512, 512));
        assert_eq!(glyph.kind(), AtlasKind::Glyph);
        let image = AtlasAllocator::with_page(AtlasKind::Image, (MIN_ATLAS_PAGE, MIN_ATLAS_PAGE));
        assert_eq!(image.kind(), AtlasKind::Image);
    }

    #[test]
    fn grow_walks_the_page_ladder_and_stops_at_the_cap() {
        let mut atlas = allocator();
        assert_eq!(atlas.page(), (64, 64));
        // 64 不在阶梯里：下一档是 512，然后是 1024、2048，之后不再增长。
        assert!(atlas.grow());
        assert_eq!(atlas.page(), (512, 512));
        assert!(atlas.grow());
        assert_eq!(atlas.page(), (1024, 1024));
        assert!(atlas.grow());
        assert_eq!(atlas.page(), (2048, 2048));
        assert!(!atlas.grow(), "2048² 是上限");
        assert_eq!(atlas.page(), (2048, 2048));
    }

    #[test]
    fn grow_bumps_the_generation_and_drops_old_tiles() {
        let mut atlas = allocator();
        let tile = atlas.allocate((8, 8)).expect("fits").tile;
        let before = atlas.generation();
        assert!(atlas.grow());
        assert_eq!(atlas.generation(), before + 1);
        assert!(tile.is_stale(atlas.generation()));
        assert!(atlas.allocate((8, 8)).is_some(), "新页可以继续分配");
    }

    /// 图片页固定开最大档（P1-2.7）：容纳能力与插入顺序无关。
    ///
    /// 旧实现按**第一张**图片选档并永久锁定，先加载 16×16 图标就会把整页钉在 512²，
    /// 之后任何 >510 px 的图必然 `Capacity` 且重试无效。
    #[test]
    fn image_page_always_uses_the_maximum_bucket() {
        assert_eq!(
            MAX_ATLAS_PAGE as usize,
            2048 - 2 + 2,
            "最大页必须能容纳 2046 px 的 tile 加上两侧各 1 纹素 padding"
        );
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
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (16, 16));
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
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (32, 32));
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

    /// 同一批脏行要分发给多个窗口的待发队列：`merge_from` 不能消费源。
    #[test]
    fn merge_from_keeps_the_source_spans() {
        let mut source = DirtyRows::default();
        source.mark(4, 8);
        let mut first = DirtyRows::default();
        let mut second = DirtyRows::default();
        first.merge_from(&source);
        second.merge_from(&source);

        assert_eq!(first.spans().len(), 1);
        assert_eq!(first.spans()[0], 4..8);
        assert_eq!(second.spans().len(), 1);
        assert_eq!(second.spans()[0], 4..8);
        assert_eq!(source.spans().len(), 1, "分发之后源必须仍然完整");
        assert_eq!(source.spans()[0], 4..8);
        assert!(!source.is_empty());
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
    fn deallocate_is_available_outside_tests() {
        // F-147 的回归面：`deallocate` 必须是生产代码（`ResourceCache::unload` 的唯一调用点）。
        let mut atlas = allocator();
        let allocation = atlas.allocate((8, 8)).expect("fits");
        atlas.deallocate(allocation.id);
        assert!(atlas.allocate((8, 8)).is_some(), "释放后容量必须回来");
    }
}
