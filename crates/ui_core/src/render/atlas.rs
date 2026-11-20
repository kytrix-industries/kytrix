//! 图集分配器（P02 §10.4 的落地）：单页 + 1 纹素 padding + 脏矩形 + 代际。
//!
//! 只做**装箱 + 脏矩形 + 代际**：不持有 CPU 像素（P02 §10.6），上传由 P09 从字形缓存读。
//!
//! 两条 probe 实测修正（phase-05 §9.2）：
//! 1. `etagere` 会把请求矩形**向上取整**（请求 8×11 得 8×16）⇒ `padded` 取 `Allocation::rectangle` 的实际值；
//! 2. 陈旧 `AllocId` 的 `deallocate` 会 `assert!` panic ⇒ [`AtlasAllocator::deallocate`] 必须自己判重。
//!
//! **脏矩形（AOI）上传**（R06-3）：脏集合是**页内矩形**而不是整页宽的行区间。单个 8×11 字形
//! 只上传 88 B（旧「整页宽 × 行区间」语义要传 11 × page 字节：512² 页上是 5632 B，64 倍放大）。
//! 合并策略见 [`DirtyRects`]（Skia `SkRegion` 的整数矩形并集思路）。

use etagere::{AllocId, AtlasAllocator as EtagereAllocator, size2};

use crate::{
    math::PhysicalRect,
    paint::atlas::{AtlasKind, AtlasTile, MAX_IMAGE_PAGES},
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
fn next_page(page: u32, max_page: u32) -> Option<u32> {
    ATLAS_PAGE_STEPS
        .iter()
        .copied()
        .filter(|candidate| *candidate <= max_page)
        .find(|candidate| *candidate > page)
}

/// 图集纹素的字节数（字形 `R8` = 1，图片 `BGRA8` = 4；三端同一契约）。
pub(crate) fn texel_bytes(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Glyph => 1,
        AtlasKind::Image => 4,
    }
}

/// 图集纹理重建位：页尺寸变化（懒创建 / 字形页换代 / 图片图集层数增长）后，调用方必须重写该
/// 图集的绑定。
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

/// 一张图集 texture array 的**运行时**规格：页边长（纹素）+ 层数。
///
/// 层数**不是**编译期常量（P2-B3 修复）：图片图集的层数 = App 侧
/// [`ImageAtlasPager::page_count`]（当前实际打开的图集页数；0 页时三端仍保留 1 层的 1×1
/// 占位数组，好让 SRV / 纹理视图始终指向有效资源），字形恒 1 层。
///
/// 层数**单调不减**：图集页槽只增不减（空页按 LRU **原地**回收复用，见
/// [`ImageAtlasPager::allocate`]），因此 `page_count` 不会回退；缩容 array 会让所有窗口
/// `AtlasTile.page` 指向不存在的层，是契约破坏。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AtlasArraySpec {
    /// 页边长（纹素；同一图集的所有页等大）。
    pub page: u32,
    /// texture array 层数（`1..=MAX_IMAGE_PAGES`；层号 = 图集页索引）。
    pub layers: u32,
}

/// 一次图集 array 重建请求：三端 `SharedGpu::rebuild_atlas_pages` 的唯一入口。
///
/// 调用方（每窗口上传器）在待上传层与当前共享 array 的 `spec` 不一致时产出请求：
/// - 页边长变化（字形页扩容阶梯 / App 换代）⇒ 换成新页尺寸；
/// - **层数增长**（图片图集开了新页）⇒ 换成更多层，旧层内容由 App 的整层重传补齐
///   （增长帧每个已打开页都会整页上传，见 `App::upload_image_page`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AtlasPageRequest {
    pub kind: AtlasKind,
    /// 目标规格：页边长 + 层数。
    pub spec: AtlasArraySpec,
}

/// 合并一条 array 重建请求：同一 kind 只留一条，层数取最大（层数只增不减）。
///
/// 三端上传器的 `reconcile_pages` 共用这一段判据：同一图集的多个待上传层会各自产出请求
/// （同一页边长、不同层数），重建只需要一次，且必须按最大的层数建数组。
pub(crate) fn merge_page_request(
    requests: &mut Vec<AtlasPageRequest>,
    kind: AtlasKind,
    requested: AtlasArraySpec,
    current: AtlasArraySpec,
) {
    let layers = requested.layers.max(current.layers);
    if let Some(existing) = requests.iter_mut().find(|request| request.kind == kind) {
        debug_assert_eq!(
            existing.spec.page, requested.page,
            "one atlas cannot rebuild to two page extents in the same frame"
        );
        existing.spec.layers = existing.spec.layers.max(layers);
        return;
    }
    requests.push(AtlasPageRequest {
        kind,
        spec: AtlasArraySpec {
            page: requested.page,
            layers,
        },
    });
}

/// 一个脏矩形在**页内**的落点 + 它在紧密打包缓冲里的字节偏移（AOI 上传契约，R06-3）。
///
/// 三后端都只消费这个视图：同一份 [`DirtyRects`] 产出同一批 `(x, y, w, h)` 区域与同一批偏移，
/// 各后端只按自己的图形 API 翻译（D3D12 `CopyTextureRegion` 的 destination box / Metal blit 的
/// origin+size / Vulkan `vkCmdCopyBufferToImage` 的 image offset+extent）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RectChunk {
    /// 页内左边界（纹素）。
    pub x: u32,
    /// 页内上边界（纹素）。
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// 该矩形在 [`pack_rects`] 输出里的起始字节偏移（按 `rects()` 顺序累计，紧密打包）。
    pub offset: usize,
}

/// 把 `page` 里 `rects` 的每个矩形子块**按行紧密打包**进 `out`（R06-3，替代旧的行区间打包）。
///
/// 布局：按 [`DirtyRects::rects`] 顺序，对每个（钳到页内的）矩形逐行拷贝 `width × texel` 字节，
/// 行间无 padding。因此 `out.len()` 恒等于 [`DirtyRects::byte_count`]，而后端可以用
/// [`DirtyRects::chunks`] 给出的 `(x, y, w, h, offset)` 做逐矩形 AOI 拷贝。`is_full()` 时等价于
/// 整页拷贝（新窗口 / 页尺寸变化 / 首次上传都走这条）。
pub(crate) fn pack_rects(
    page: &[u8],
    extent: u32,
    texel: u32,
    rects: &DirtyRects,
    out: &mut Vec<u8>,
) {
    let expected = extent as usize * extent as usize * texel as usize;
    assert_eq!(
        page.len(),
        expected,
        "atlas page buffer must be {extent}² × {texel} bytes"
    );
    out.clear();
    if rects.is_full() {
        out.extend_from_slice(page);
        return;
    }
    let row_bytes = extent as usize * texel as usize;
    for chunk in rects.chunks(extent, texel) {
        let width_bytes = chunk.width as usize * texel as usize;
        let start = chunk.y as usize * row_bytes + chunk.x as usize * texel as usize;
        for row in 0..chunk.height as usize {
            let source = start + row * row_bytes;
            out.extend_from_slice(&page[source..source + width_bytes]);
        }
        debug_assert_eq!(
            out.len(),
            chunk.offset + width_bytes * chunk.height as usize,
            "packed layout must match the chunk offsets the backends consume"
        );
    }
}

/// 一个 `AtlasKind` 的**单页**分配器（P02 §10.4）。
pub(crate) struct AtlasAllocator {
    /// 页档阶梯的上限（生产 = `MAX_ATLAS_PAGE`；测试可以调小，用于在几十个 tile 内验证
    /// 逐 tile LRU 淘汰与软失败，而不必真的塞满一张 2048² 页）。
    max_page: u32,
    kind: AtlasKind,
    page: (u32, u32),
    /// 该分配器对应的图集页索引（D-9 多页图集）：写进 [`AtlasTile::page`]。
    ///
    /// 单页图集（字形、以及多页改造前的图片）恒为 0；图片图集多页化后由页管理器设置。
    page_index: u16,
    allocator: EtagereAllocator,
    /// 槽位表：`deallocate` 的幂等守卫（`etagere` 对陈旧 `AllocId` 会 `assert!` panic）。
    ///
    /// P13 起是**生产**字段：`ResourceCache::unload` 需要真正逐块释放（F-147）。
    tiles: Vec<Option<AllocId>>,
    free: Vec<usize>,
    generation: u32,
    dirty: DirtyRects,
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
            max_page: MAX_ATLAS_PAGE,
            kind,
            page,
            page_index: 0,
            allocator: EtagereAllocator::new(size2(page.0 as i32, page.1 as i32)),
            tiles: Vec::with_capacity(64),
            free: Vec::new(),
            generation: 1,
            dirty: DirtyRects::default(),
        }
    }

    /// 设置该分配器对应的图集页索引（D-9 多页图片图集用；字形图集恒 0）。
    pub(crate) fn set_page_index(&mut self, page_index: u16) {
        self.page_index = page_index;
    }

    /// 仅测试：把页档上限钉在 `page`（调小阶梯，用来在少量 tile 内触发 LRU 淘汰/软失败）。
    #[cfg(test)]
    pub(crate) fn cap_page_for_test(&mut self, page: u32) {
        debug_assert!(page > 0);
        self.max_page = page;
        self.page = (page, page);
        self.clear();
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
            page: self.page_index,
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

    /// 该 `AllocId` 当前是否还活着（[`ImageAtlasPager`] 用它做幂等的活 tile 计数）。
    pub(crate) fn is_live(&self, id: AllocId) -> bool {
        self.tiles
            .iter()
            .any(|slot| matches!(slot, Some(existing) if *existing == id))
    }

    /// 仅探测：`content`（+padding）当前能否在本页分配。**不改动** tiles / 脏矩形 / 代际。
    ///
    /// 实现是「分配后立刻归还」：`etagere` 没有只读的 fit 查询；归还后 free list 与本页原状态
    /// 等价（同一块矩形回到空闲表），不会泄漏或改变语义。仅供 [`ImageAtlasPager::would_open_new_page`]
    /// 在**解码前预算准入**里判断是否可能新开一页（RD-2）；不在热路径。
    pub(crate) fn has_room_for(&mut self, content: (u32, u32)) -> bool {
        let (width, height) = content;
        let Some(padded) = width.checked_add(2).zip(height.checked_add(2)) else {
            return false;
        };
        if padded.0 > self.page.0 || padded.1 > self.page.1 {
            return false;
        }
        let Some(allocation) = self
            .allocator
            .allocate(size2(padded.0 as i32, padded.1 as i32))
        else {
            return false;
        };
        self.allocator.deallocate(allocation.id);
        true
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
        let Some(next) = next_page(self.page.0, self.max_page) else {
            return false;
        };
        self.page = (next, next);
        self.clear();
        true
    }

    /// 记录「这块矩形被写脏了」（P02 §10.5，R06-3 起是**矩形**而不是整页宽的行区间）。
    ///
    /// 旧实现把 `rect` 的 x 区间丢掉、只留 `y0..y1`，于是单个 8×11 字形也要上传一整段页宽的行。
    pub(crate) fn mark_written(&mut self, rect: PhysicalRect) {
        self.dirty.mark(rect);
    }

    /// 取走脏矩形（P09 的增量上传目标）。`App::layout_text` 把它合并进 `App::pending_dirty`。
    pub(crate) fn take_dirty_rects(&mut self) -> DirtyRects {
        core::mem::take(&mut self.dirty)
    }
}

/// 图片图集的多页分配器（D-9）：`MAX_IMAGE_PAGES` 页、按需懒开、空闲页 LRU 回收。
///
/// 语义（用户裁决「多页图集 + LRU」，取代审计 D-9 的单页容量文档方案）：
/// - 每页一个 `MAX_ATLAS_PAGE²` 的 [`AtlasAllocator`]；`AtlasTile.page` 是页号
///   （`0..page_count`），三端用 `GpuPrimitive.params1.y` 作 texture array 的层号。
/// - **`page_count()` 就是三端图片图集 texture array 的层数**：没有图片时是 0（三端保留 1 层的
///   1×1 占位数组），每开一页层数 +1，App 侧把它作为 `AtlasArraySpec::layers` 下发给渲染器。
/// - 分配先试现有页；页满且页数未达上限 ⇒ 开新页（新页整页标记脏：三端首次必须整层上传）；
///   页数达上限时，若存在**没有活 tile** 的页 ⇒ 按 LRU 复用最久未用的空页（`clear` 换代后
///   原地重新分配，页号稳定）；否则 `None`（上层报 `Capacity{PageFull}`）。
/// - `deallocate` 只释放单块；页变空后进入空闲表，等下一次分配回收。
/// - **活 tile 永不淘汰**（tile 恒有效契约）：LRU 只作用于已空的页。
/// - **页槽只增不减**：`page_count()` 单调不减（回收是原地复用，不是删除页槽）⇒ 数组层数
///   单调不减，缩容（会让已有 `AtlasTile.page` 失效）在结构上不可能发生。
pub(crate) struct ImageAtlasPager {
    /// 页槽：下标 == 页号。页槽只增不减（页号稳定；回收 = `clear` 原地复用）。
    pages: Vec<PagerPage>,
    /// 空闲页号（按 `last_used` 升序选最久未用的回收）。
    idle: Vec<u16>,
    /// 每页的边长（纹素）。生产恒 `MAX_ATLAS_PAGE`；测试用小页验证容量/回收。
    page_size: u32,
    /// 单调 LRU 时钟（每次分配 +1）。
    clock: u64,
}

struct PagerPage {
    allocator: AtlasAllocator,
    /// 活 tile 数（0 = 页空，可回收）。
    live: usize,
    /// 最近一次分配的时钟值（LRU 回收顺序）。
    last_used: u64,
}

impl ImageAtlasPager {
    pub(crate) fn new() -> Self {
        Self::with_page_size(MAX_ATLAS_PAGE)
    }

    fn with_page_size(page_size: u32) -> Self {
        debug_assert!(page_size > 0, "atlas page must be non-empty");
        Self {
            pages: Vec::with_capacity(usize::from(MAX_IMAGE_PAGES)),
            idle: Vec::new(),
            page_size,
            clock: 0,
        }
    }

    /// 仅测试：用小页（几十个 tile 即填满）验证页面增长 / 容量上限 / LRU 回收，避免真的塞满
    /// 4 × 2048² 页。
    #[cfg(test)]
    pub(crate) fn with_page_for_test(page_size: u32) -> Self {
        Self::with_page_size(page_size)
    }

    /// 已打开的页数（含空闲页；页缓冲 / GPU texture array 的层数按这个数常驻）。
    ///
    /// **单调不减**：空页按 LRU 原地回收（`clear` 换代后复用同一个页槽），页槽不会被删除。
    /// 这就是三端图片图集 texture array 的层数来源（见 [`AtlasArraySpec`]）。
    pub(crate) fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// 单页边长（纹素；所有页相同）。
    pub(crate) fn page_size(&self) -> u32 {
        self.page_size
    }

    /// 仅测试：预开第 0 页并整页标脏（等价于旧单页实现的「构造时建页」语义，让既有页容量/
    /// 脏矩形测试保持原有口径）。
    #[cfg(test)]
    pub(crate) fn preopen_page_for_test(&mut self) {
        debug_assert!(self.pages.is_empty(), "只用于构造期预开页");
        let mut allocator =
            AtlasAllocator::with_page(AtlasKind::Image, (self.page_size, self.page_size));
        allocator.set_page_index(0);
        allocator.mark_written(full_page(self.page_size));
        self.pages.push(PagerPage {
            allocator,
            live: 0,
            last_used: 1,
        });
        // 预开页没有活 tile ⇒ 进入空闲表：首次分配走「清空换代 + 原地复用」路径，
        // 与生产「空页按 LRU 回收」是同一条代码路径（测试因此覆盖真实回收逻辑）。
        self.idle.push(0);
        self.clock = 1;
    }

    /// 分配 `content` 是否**必然新开一页**（页数 +1；命中现有页空闲空间或 LRU 复用空页都不算）。
    ///
    /// RD-2：资源层在**解码前预算准入**时用它判断是否要额外预留一页 CPU 缓冲（16 MiB）——
    /// 旧实现只在 `page_count()==0` 时预留一页，已开页后再新开页可让 `used_bytes` 超出预算
    /// 最多 3 页（48 MiB）。estimate 只需保守（按源尺寸 clamp 到 `MAX_TILE_DIM`），
    /// 多预留只会让预算更严格、不会漏。
    pub(crate) fn would_open_new_page(&mut self, content: (u32, u32)) -> bool {
        let (width, height) = content;
        if width == 0 || height == 0 {
            return false;
        }
        let Some(padded) = width.checked_add(2).zip(height.checked_add(2)) else {
            return false;
        };
        if padded.0 > self.page_size || padded.1 > self.page_size {
            return false;
        }
        // 非空页有空闲空间 ⇒ 不开新页。
        for index in 0..self.pages.len() {
            if self.pages[index].live == 0 {
                continue;
            }
            if self.pages[index].allocator.has_room_for(content) {
                return false;
            }
        }
        // 有空闲页 ⇒ 原地回收复用，页数不变。
        if !self.idle.is_empty() {
            return false;
        }
        self.pages.len() < usize::from(MAX_IMAGE_PAGES)
    }

    /// 分配一块内容区（1 纹素 padding 由 [`AtlasAllocator`] 加）。返回的 `tile.page` 已设置。
    pub(crate) fn allocate(&mut self, content: (u32, u32)) -> Option<Allocation> {
        let (width, height) = content;
        if width == 0 || height == 0 {
            return None;
        }
        // 页级上限先判：内容区 + padding 超过页长时开新页也放不下（避免为无效分配白开一页）。
        let padded = (width.checked_add(2)?, height.checked_add(2)?);
        if padded.0 > self.page_size || padded.1 > self.page_size {
            return None;
        }
        // 1) 先复用**非空页**的空闲空间（空页留到回收路径做换代，避免装箱碎片累积）。
        for index in 0..self.pages.len() {
            if self.pages[index].live == 0 {
                continue;
            }
            if let Some(allocation) = self.allocate_on(index, content) {
                return Some(allocation);
            }
        }
        // 2) 页满：优先回收最久未用的空页（换代后原地复用；先于开新页，避免白占一页内存）。
        if let Some(idle_index) = self
            .idle
            .iter()
            .copied()
            .min_by_key(|page| self.pages[usize::from(*page)].last_used)
        {
            let index = usize::from(idle_index);
            let slot = &mut self.pages[index];
            // 回收 = 原地换代：`clear` 重置装箱表/脏矩形并 generation+1；旧 tile 已全部释放
            // （页空才可回收），因此不会破坏 tile 恒有效契约。
            slot.allocator.clear();
            slot.allocator.set_page_index(idle_index);
            slot.live = 0;
            self.idle.retain(|page| *page != idle_index);
            return self.allocate_on(index, content);
        }
        // 3) 开新页（未达上限）；新页整页标记脏：三端首次必须整层上传。
        if self.pages.len() < usize::from(MAX_IMAGE_PAGES) {
            let index = self.pages.len();
            let mut allocator =
                AtlasAllocator::with_page(AtlasKind::Image, (self.page_size, self.page_size));
            allocator.set_page_index(u16::try_from(index).ok()?);
            allocator.mark_written(FULL_RECT);
            self.pages.push(PagerPage {
                allocator,
                live: 0,
                last_used: 0,
            });
            return self.allocate_on(index, content);
        }
        None
    }

    /// 在指定页上分配并记账（`tile.page` 与 LRU 时钟）。
    fn allocate_on(&mut self, index: usize, content: (u32, u32)) -> Option<Allocation> {
        let mut allocation = self.pages[index].allocator.allocate(content)?;
        allocation.tile.page = u16::try_from(index).ok()?;
        self.pages[index].live += 1;
        self.clock = self.clock.wrapping_add(1);
        self.pages[index].last_used = self.clock;
        Some(allocation)
    }

    /// 释放一块（幂等：陈旧 `AllocId` 不改变活 tile 计数）。页变空后进入空闲表。
    pub(crate) fn deallocate(&mut self, page: u16, id: AllocId) {
        let Some(slot) = self.pages.get_mut(usize::from(page)) else {
            return;
        };
        let was_live = slot.allocator.is_live(id);
        slot.allocator.deallocate(id);
        if !was_live {
            return;
        }
        slot.live -= 1;
        if slot.live == 0 && !self.idle.contains(&page) {
            self.idle.push(page);
        }
    }

    /// 记录某页的写脏矩形（资源层把解码后的像素写进 CPU 页缓冲后调用）。
    pub(crate) fn mark_written(&mut self, page: u16, rect: PhysicalRect) {
        if let Some(slot) = self.pages.get_mut(usize::from(page)) {
            slot.allocator.mark_written(rect);
        }
    }

    /// 取走某页的待上传矩形（`None` = 页号无效）。首次（新页）返回整页哨兵。
    pub(crate) fn take_dirty_rects(&mut self, page: u16) -> Option<DirtyRects> {
        self.pages
            .get_mut(usize::from(page))
            .map(|slot| slot.allocator.take_dirty_rects())
    }

    /// 某页的分配代际（诊断/测试：页回收复用后必须换代）。
    #[must_use]
    pub(crate) fn generation(&self, page: u16) -> Option<u32> {
        self.pages
            .get(usize::from(page))
            .map(|slot| slot.allocator.generation())
    }
}

impl Default for ImageAtlasPager {
    fn default() -> Self {
        Self::new()
    }
}

/// 整页哨兵：覆盖任意页面的矩形（沿用旧 `mark(0, u32::MAX)` 的语义）。
///
/// 消费方仍靠 [`DirtyRects::is_full`] 走「整页缓冲直接上传、不打包」的快路径。
const FULL_RECT: PhysicalRect = PhysicalRect {
    x0: 0,
    y0: 0,
    x1: i32::MAX,
    y1: i32::MAX,
};

/// 一整页的矩形（测试预开页的「整页脏」口径）。
#[cfg(test)]
fn full_page(page: u32) -> PhysicalRect {
    PhysicalRect::new(0, 0, page as i32, page as i32)
}

/// 被写脏的**矩形**集合（Skia `SkRegion` 的整数矩形并集思路）：多个页内物理像素矩形，
/// 按 `(y0, x0)` 升序、已合并。
///
/// 合并策略（纯函数式：无 I/O、无全局状态）：
/// - 新矩形被已有矩形**包含** ⇒ 直接忽略（不增长集合）；
/// - 新矩形与已有矩形**相交或相邻**（半开区间下边接触也算相邻）⇒ 迭代吸收成外接矩形直到不动点
///   （吸收后的外接矩形可能又贴上下一个）；
/// - 矩形数超过 [`DirtyRects::MAX_RECTS`]（沿用旧上限 8）⇒ 折叠为整页哨兵 [`FULL_RECT`]。
///
/// 复杂度：集合上限 8 ⇒ `mark` 的最坏 O(n²)（n ≤ 8）次矩形比较，摊销 O(1)；
/// `chunks` / `byte_count` / `pack_rects` 都是 O(矩形数) + O(打包字节数)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DirtyRects {
    rects: Vec<PhysicalRect>,
}

impl DirtyRects {
    /// 段/矩形数上限：超过就折叠为并集外接矩形（沿用旧 `DirtyRects` 的 8，避免碎片化上传）。
    ///
    /// 折叠目标**不是**整页哨兵（X4）：整页上传是 16 MiB 级拷贝/尖峰，只在显式整页
    /// （首帧 / 页尺寸变化 / 渲染器重建）时使用。
    const MAX_RECTS: usize = 8;

    /// 整页哨兵集合（等价于旧的 `mark(0, u32::MAX)`）。
    pub(crate) fn full() -> Self {
        Self {
            rects: vec![FULL_RECT],
        }
    }

    /// 标记 `rect` 脏；空矩形忽略；被已有矩形包含则忽略；相交/相邻合并；超上限折叠为 bbox。
    pub(crate) fn mark(&mut self, rect: PhysicalRect) {
        if rect.is_empty() || self.is_full() {
            return;
        }
        // 被已有矩形包含 ⇒ 本次上传不需要新的 AOI 区域（否则集合只会碎片化）。
        if self.rects.iter().any(|existing| contains(*existing, rect)) {
            return;
        }
        let mut merged = rect;
        // 不动点：吸收所有相交/相邻的矩形。外接矩形变大后可能贴上下一个，因此必须重扫。
        loop {
            let mut absorbed = false;
            let mut index = 0;
            while index < self.rects.len() {
                if touches(merged, self.rects[index]) {
                    merged = merged.union(self.rects[index]);
                    self.rects.swap_remove(index);
                    absorbed = true;
                } else {
                    index += 1;
                }
            }
            if !absorbed {
                break;
            }
        }
        self.rects.push(merged);
        self.rects.sort_by_key(|rect| (rect.y0, rect.x0));
        if self.rects.len() > Self::MAX_RECTS {
            // X4：折叠为**并集外接矩形**（bounding box）而不是整页哨兵。旧行为让「一帧 ≥9 块
            // 图片」必然整页上传（2048² BGRA8 = 16 MiB UI 线程拷贝 + 上传尖峰）；bbox 面积恒
            // ≤ 页面，三端 AOI 契约不变（仍按矩形盒拷贝）。
            let mut union = self.rects[0];
            for rect in &self.rects[1..] {
                union = union.union(*rect);
            }
            self.rects.clear();
            self.rects.push(union);
        }
    }

    /// 本次上传的矩形（升序；顺序 = 打包顺序 = 后端 AOI 顺序）。
    pub(crate) fn rects(&self) -> &[PhysicalRect] {
        &self.rects
    }

    /// 逐矩形产出**页内**落点 + 打包偏移；越页部分被钳掉，空块跳过。
    ///
    /// 这是三后端唯一的 AOI 契约来源：`(x, y, width, height)` 相同 ⇒ 三端上传同一批区域。
    /// `offset` 与 [`pack_rects`] 的打包布局逐个对应（紧密、无 padding）。
    pub(crate) fn chunks(&self, page: u32, texel: u32) -> impl Iterator<Item = RectChunk> + '_ {
        let limit = page.min(i32::MAX as u32) as i32;
        let mut offset = 0usize;
        self.rects.iter().filter_map(move |rect| {
            let x0 = rect.x0.clamp(0, limit) as u32;
            let y0 = rect.y0.clamp(0, limit) as u32;
            let x1 = rect.x1.clamp(0, limit) as u32;
            let y1 = rect.y1.clamp(0, limit) as u32;
            if x0 >= x1 || y0 >= y1 {
                return None;
            }
            let chunk = RectChunk {
                x: x0,
                y: y0,
                width: x1 - x0,
                height: y1 - y0,
                offset,
            };
            offset += (chunk.width * chunk.height) as usize * texel as usize;
            Some(chunk)
        })
    }

    /// 本次上传的字节数（逐矩形钳到页内；整页 = `page² × texel`）。
    ///
    /// 必须与 [`pack_rects`] 的输出长度**逐字节一致**：后端用它校验载荷、测试用它断言放大倍数。
    pub(crate) fn byte_count(&self, page: u32, texel: u32) -> usize {
        self.chunks(page, texel)
            .map(|chunk| (chunk.width * chunk.height) as usize * texel as usize)
            .sum()
    }

    /// 是否恰好覆盖整页（页换代之后「整页上传」的判据；整页哨兵也算）。
    pub(crate) fn covers_page(&self, page: u32) -> bool {
        let page = page.min(i32::MAX as u32) as i32;
        self.rects
            .iter()
            .any(|rect| rect.x0 <= 0 && rect.y0 <= 0 && rect.x1 >= page && rect.y1 >= page)
    }

    /// 是否退化为「整页」。
    ///
    /// 只有 [`FULL_RECT`] 的右/下边是 `i32::MAX`（`PhysicalRect` 不变量保证不会超过它），
    /// 因此「覆盖哨兵」等价于「左上不窄于哨兵、右下正好到哨兵」。
    pub(crate) fn is_full(&self) -> bool {
        self.rects.iter().any(|rect| {
            rect.x0 <= FULL_RECT.x0
                && rect.y0 <= FULL_RECT.y0
                && rect.x1 == FULL_RECT.x1
                && rect.y1 == FULL_RECT.y1
        })
    }

    /// 没有任何脏矩形。
    pub(crate) fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.rects.clear();
    }

    /// 合并另一份脏矩形（P05 把每轮 `take_dirty_rects` 的结果并入 `App::pending_dirty`）。
    pub(crate) fn merge(&mut self, other: &mut Self) {
        let other = core::mem::take(other);
        for rect in other.rects {
            self.mark(rect);
        }
    }

    /// 只读合并：把 `other` 的矩形并进来，但不清空 `other`。
    ///
    /// 图集页是进程级共享数据、纹理是每窗口一份：同一批脏矩形要分发给每个窗口的待发队列
    /// （忙窗口攒着、空闲时补发），因此不能像 [`DirtyRects::merge`] 那样消费源。
    pub(crate) fn merge_from(&mut self, other: &Self) {
        for rect in &other.rects {
            self.mark(*rect);
        }
    }
}

/// `outer` 是否包含 `inner`（半开区间；相等也算包含）。
fn contains(outer: PhysicalRect, inner: PhysicalRect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

/// 两个矩形是否**相交或相邻**（半开区间：`a.x1 == b.x0` 的边接触也算相邻）。
fn touches(a: PhysicalRect, b: PhysicalRect) -> bool {
    a.x0 <= b.x1 && b.x0 <= a.x1 && a.y0 <= b.y1 && b.y0 <= a.y1
}

#[cfg(test)]
mod tests {
    use super::{
        ATLAS_PAGE_STEPS, AtlasAllocator, DirtyRects, ImageAtlasPager, MAX_ATLAS_PAGE,
        MIN_ATLAS_PAGE, RectChunk, pack_rects,
    };
    use crate::{
        math::PhysicalRect,
        paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
    };

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

    /// 相邻（边接触）与重叠都合并成外接矩形；包含则忽略；不相交保留。
    #[test]
    fn dirty_rects_merge_adjacent_and_ignore_contained_rects() {
        let mut rects = DirtyRects::default();
        // 相邻（边接触）+ 重叠 + 不相交：前两块并成 (0,10)..(16,20)，第三块独立。
        rects.mark(PhysicalRect::new(0, 10, 8, 20));
        rects.mark(PhysicalRect::new(8, 10, 16, 20));
        rects.mark(PhysicalRect::new(0, 10, 16, 20));
        rects.mark(PhysicalRect::new(100, 200, 108, 211));
        assert_eq!(rects.rects().len(), 2);
        assert_eq!(rects.rects()[0], PhysicalRect::new(0, 10, 16, 20));
        assert_eq!(rects.rects()[1], PhysicalRect::new(100, 200, 108, 211));
        assert!(!rects.is_full());

        // 被已有矩形包含 ⇒ 直接忽略（集合不增长，也没多出上传区域）。
        rects.mark(PhysicalRect::new(2, 12, 6, 18));
        assert_eq!(rects.rects().len(), 2);
        assert_eq!(rects.byte_count(512, 1), 16 * 10 + 8 * 11);

        // 与之相交的更大矩形会吸收它（外接矩形）。
        rects.mark(PhysicalRect::new(-4, 8, 4, 12));
        assert_eq!(rects.rects().len(), 2);
        assert_eq!(rects.rects()[0], PhysicalRect::new(-4, 8, 16, 20));
    }

    #[test]
    fn dirty_rects_fold_to_a_bounding_box_when_the_limit_is_exceeded_and_clear_empties() {
        let mut rects = DirtyRects::default();
        for index in 0..DirtyRects::MAX_RECTS {
            let y = index as i32 * 16;
            rects.mark(PhysicalRect::new(0, y, 4, y + 4));
            assert_eq!(rects.rects().len(), index + 1, "第 {index} 块必须保留");
        }
        assert!(!rects.is_full());
        rects.mark(PhysicalRect::new(0, 4096, 4, 4100));
        // X4：超过上限折叠为**并集外接矩形**，不是整页哨兵（整页 = 16 MiB 级上传尖峰）。
        assert!(!rects.is_full(), "折叠后不得退化为整页哨兵");
        assert_eq!(rects.rects(), &[PhysicalRect::new(0, 0, 4, 4100)]);
        // 页高 512：yte_count 按页裁剪（y1 4096→512），= 4 × 512 × 1 B。
        assert_eq!(rects.byte_count(512, 1), 4 * 512);

        rects.clear();
        assert!(rects.is_empty());
        assert!(rects.rects().is_empty());
    }

    /// X4：一帧 9+ 块分散的图片矩形不得折叠成整页上传（旧行为 16 MiB 拷贝/尖峰）；
    /// bbox 面积恒 ≤ 页面，上传字节数必然小于整页。
    #[test]
    fn nine_scattered_image_rects_fold_to_a_bounding_box_not_the_whole_page() {
        const PAGE: u32 = 2048;
        const TEXEL: u32 = 4;
        let mut rects = DirtyRects::default();
        // 9 块 32² 图片（+2 padding 的 34² 块）沿对角线/网格分散。
        let mut expected: Option<PhysicalRect> = None;
        for index in 0..9i32 {
            let x = (index % 3) * 600;
            let y = (index / 3) * 600;
            let rect = PhysicalRect::new(x, y, x + 34, y + 34);
            rects.mark(rect);
            expected = Some(match expected {
                Some(union) => union.union(rect),
                None => rect,
            });
            if index < 8 {
                assert!(!rects.is_full(), "未超上限前不得整页");
            }
        }
        assert!(
            !rects.is_full(),
            "超过 8 块后必须折叠为 bbox，而不是整页哨兵"
        );
        assert_eq!(rects.rects(), &[expected.expect("有矩形")]);
        let bytes = rects.byte_count(PAGE, TEXEL) as u64;
        let page_bytes = u64::from(PAGE) * u64::from(PAGE) * u64::from(TEXEL);
        assert!(
            bytes < page_bytes,
            "bbox 上传字节 {bytes} 必须小于整页 {page_bytes}（旧实现折叠整页）"
        );
        assert_eq!(rects.chunks(PAGE, TEXEL).count(), 1, "bbox 是单个矩形块");
    }

    #[test]
    fn empty_rects_are_ignored_and_an_empty_set_uploads_nothing() {
        let mut rects = DirtyRects::default();
        rects.mark(PhysicalRect::new(10, 10, 10, 20));
        rects.mark(PhysicalRect::new(10, 20, 20, 20));
        assert!(rects.is_empty());
        assert_eq!(rects.byte_count(512, 1), 0);
        assert_eq!(rects.chunks(512, 1).count(), 0);

        // 空集合的打包输出是空的：调用方据此跳过 staging 分配 / copy 录制。
        let page = vec![0u8; 512 * 512];
        let mut out = vec![7u8; 4];
        pack_rects(&page, 512, 1, &rects, &mut out);
        assert!(out.is_empty());
    }

    /// 同一批脏矩形要分发给多个窗口的待发队列：`merge_from` 不能消费源。
    #[test]
    fn merge_from_keeps_the_source_rects() {
        let mut source = DirtyRects::default();
        source.mark(PhysicalRect::new(0, 4, 8, 8));
        let mut first = DirtyRects::default();
        let mut second = DirtyRects::default();
        first.merge_from(&source);
        second.merge_from(&source);

        assert_eq!(first.rects(), source.rects());
        assert_eq!(second.rects(), source.rects());
        assert_eq!(source.rects().len(), 1, "分发之后源必须仍然完整");
        assert!(!source.is_empty());

        // `merge` 是消费源的另一半契约：并进自己之后源清空。
        first.merge(&mut source);
        assert!(source.is_empty(), "merge 必须消费源");
        assert_eq!(first.rects().len(), 1);
    }

    /// 根因回归（R06-3）：`mark_written` 必须保留矩形的 x 区间。
    ///
    /// 旧实现只取 `y0..y1`，于是 8×11 的字形被放大成「11 整行 × 页宽」。这里直接断言分配出的
    /// 内容矩形（位置 + 尺寸）就是脏矩形。
    #[test]
    fn mark_written_keeps_the_full_rect_not_just_the_rows() {
        let mut atlas = allocator();
        let allocation = atlas.allocate((5, 7)).expect("fits");
        let rects = atlas.take_dirty_rects();
        assert_eq!(rects.rects(), &[allocation.content]);
        assert_eq!(
            rects.rects()[0].x0,
            allocation.content.x0,
            "x 起点必须保留（旧实现丢掉 x0/x1）"
        );
        assert_eq!(rects.rects()[0].width(), 5, "宽度必须是内容宽，不是页宽");
        assert!(atlas.take_dirty_rects().is_empty(), "take drains");
    }

    /// 字节下降定量断言（R06-3 的核心证据）：512² 字形页里写一个 8×11 字形。
    ///
    /// 旧「整页宽 × 行区间」语义：11 行 × 页宽 512 = 5632 B；AOI：8×11 = 88 B（**64 倍**）。
    #[test]
    fn an_eight_by_eleven_glyph_uploads_88_bytes_instead_of_5632() {
        const PAGE: u32 = 512;
        let mut rects = DirtyRects::default();
        rects.mark(PhysicalRect::new(100, 200, 108, 211));
        assert_eq!(rects.byte_count(PAGE, 1), 88, "AOI 字节数");
        let legacy = 11 * PAGE as usize;
        assert_eq!(legacy, 5632, "旧语义的行区间字节数");
        assert_eq!(legacy / rects.byte_count(PAGE, 1), 64, "放大倍数");
        // 页档越大放大越夸张：1024² 是 128 倍，2048² 字形页上同一字形要 11 × 2048 = 22528 B
        // ⇒ 256 倍。台账的「单字形放大 220×」正落在这一区间内。
        assert_eq!(11 * 1024 / rects.byte_count(1024, 1), 128);
        assert_eq!(11 * 2048 / rects.byte_count(2048, 1), 256);

        // 打包输出长度 = 字节计数，且内容只来自该矩形（按行紧密排列）。
        let mut page = vec![0u8; (PAGE * PAGE) as usize];
        for y in 200..211usize {
            for x in 100..108usize {
                page[y * PAGE as usize + x] = 0xAB;
            }
        }
        let mut out = Vec::new();
        pack_rects(&page, PAGE, 1, &rects, &mut out);
        assert_eq!(out.len(), 88);
        assert!(out.iter().all(|byte| *byte == 0xAB));

        // 三个后端用的 chunk 视图：一个 8×11 的 AOI 区域、偏移 0。
        let chunks: Vec<RectChunk> = rects.chunks(PAGE, 1).collect();
        assert_eq!(
            chunks,
            vec![RectChunk {
                x: 100,
                y: 200,
                width: 8,
                height: 11,
                offset: 0,
            }]
        );
    }

    /// 图片页同法：16×16 的 BGRA8 图片（512² 页）。
    ///
    /// 旧语义：16 行 × 页宽 512 × 4 B = 32768 B；AOI：16×16×4 = 1024 B（**32 倍**）。
    #[test]
    fn a_sixteen_by_sixteen_image_uploads_1024_bytes_instead_of_32768() {
        const PAGE: u32 = 512;
        const TEXEL: u32 = 4;
        let mut rects = DirtyRects::default();
        rects.mark(PhysicalRect::new(64, 128, 80, 144));
        let bytes = rects.byte_count(PAGE, TEXEL);
        assert_eq!(bytes, 16 * 16 * 4);
        let legacy = 16 * PAGE as usize * TEXEL as usize;
        assert_eq!(legacy, 32768);
        assert_eq!(legacy / bytes, 32, "放大倍数");

        let mut page = vec![0u8; (PAGE * PAGE) as usize * TEXEL as usize];
        for y in 128..144usize {
            for x in 64..80usize {
                let start = (y * PAGE as usize + x) * TEXEL as usize;
                page[start..start + 4].copy_from_slice(&[0x11, 0x22, 0x33, 0xFF]);
            }
        }
        let mut out = Vec::new();
        pack_rects(&page, PAGE, TEXEL, &rects, &mut out);
        assert_eq!(out.len(), bytes);
        // 行内只拷 width × texel 字节：每行开头就是图片第一行第一个纹素。
        assert_eq!(&out[..4], &[0x11, 0x22, 0x33, 0xFF]);
        assert_eq!(&out[16 * 4 - 4..16 * 4], &[0x11, 0x22, 0x33, 0xFF]);
    }

    /// 多矩形打包布局与 `chunks` 的偏移逐个对应（后端按它做逐矩形 copy）。
    #[test]
    fn multi_rect_packing_matches_the_chunk_offsets() {
        const PAGE: u32 = 64;
        let first = PhysicalRect::new(0, 10, 8, 12); // 8×2 = 16 B
        let second = PhysicalRect::new(20, 10, 24, 13); // 4×3 = 12 B
        let third = PhysicalRect::new(40, 40, 48, 41); // 8×1 = 8 B
        let mut rects = DirtyRects::default();
        rects.mark(first);
        rects.mark(second);
        rects.mark(third);
        // 非均匀页内容（AR5 假绿 2）：每个纹素一个可区分的字节。旧测试的页全是 0xFF，
        // 于是「长度对、偏移对、内容全错」（打包行起点丢掉 chunk.x）也能变绿。
        let page: Vec<u8> = (0..(PAGE * PAGE))
            .map(|index| (index % 251) as u8)
            .collect();
        let mut out = Vec::new();
        pack_rects(&page, PAGE, 1, &rects, &mut out);
        assert_eq!(out.len(), rects.byte_count(PAGE, 1));
        assert_eq!(out.len(), 36);
        let chunks: Vec<RectChunk> = rects.chunks(PAGE, 1).collect();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.offset).collect::<Vec<_>>(),
            vec![0, 16, 28]
        );

        // 期望值由源页 + **显式矩形坐标**独立算出（不经过 `chunks`/`byte_count`，避免同源自证），
        // 然后逐字节比较：行 stride、行内宽度、x 起点任何一处写错都会在这里失败。
        let expected_block = |rect: PhysicalRect| -> Vec<u8> {
            let mut block = Vec::with_capacity(rect.width() as usize * rect.height() as usize);
            for y in rect.y0..rect.y1 {
                for x in rect.x0..rect.x1 {
                    block.push(page[y as usize * PAGE as usize + x as usize]);
                }
            }
            block
        };
        let blocks = [first, second, third].map(expected_block);
        assert_eq!(blocks.concat(), out, "打包内容必须逐字节等于源页对应区域");
        for (chunk, block) in chunks.iter().zip(&blocks) {
            assert_eq!(
                &out[chunk.offset..chunk.offset + block.len()],
                block.as_slice(),
                "chunk {chunk:?} 的偏移必须指向它自己的打包块（x 起点写错会在这里现形）"
            );
        }
    }

    /// 整页哨兵：`full()` 覆盖全页、`byte_count` 等于整页字节、打包输出就是整页。
    #[test]
    fn the_full_sentinel_uploads_exactly_one_page() {
        let rects = DirtyRects::full();
        assert!(rects.is_full());
        assert!(rects.covers_page(512));
        assert_eq!(rects.byte_count(512, 1), 512 * 512);
        assert_eq!(rects.byte_count(512, 4), 512 * 512 * 4);
        assert_eq!(
            rects.chunks(512, 1).collect::<Vec<_>>(),
            vec![RectChunk {
                x: 0,
                y: 0,
                width: 512,
                height: 512,
                offset: 0,
            }]
        );
        let page = vec![0x5Au8; 512 * 512];
        let mut out = Vec::new();
        pack_rects(&page, 512, 1, &rects, &mut out);
        assert_eq!(out, page);
    }

    /// 越页矩形被钳到页内（脏矩形来自分配器，理论上总在页内；钳位是后端的最后防线）。
    #[test]
    fn chunks_clamp_rects_to_the_page() {
        let mut rects = DirtyRects::default();
        rects.mark(PhysicalRect::new(-4, -4, 6, 6));
        assert_eq!(
            rects.chunks(512, 1).collect::<Vec<_>>(),
            vec![RectChunk {
                x: 0,
                y: 0,
                width: 6,
                height: 6,
                offset: 0,
            }]
        );
        assert_eq!(rects.byte_count(512, 1), 36);

        let mut off_page = DirtyRects::default();
        off_page.mark(PhysicalRect::new(600, 600, 700, 700));
        assert_eq!(off_page.byte_count(512, 1), 0, "页外矩形不上传");
        assert_eq!(off_page.chunks(512, 1).count(), 0);

        // 跨右/下边界（AR5 假绿 1）：钳位必须同时作用于 x1/y1，而不只是负坐标。
        // 页 512²：500..700 两个方向都钳到 500..512（12×12）。
        let mut crossing = DirtyRects::default();
        crossing.mark(PhysicalRect::new(500, 500, 700, 700));
        assert_eq!(
            crossing.chunks(512, 1).collect::<Vec<_>>(),
            vec![RectChunk {
                x: 500,
                y: 500,
                width: 12,
                height: 12,
                offset: 0,
            }],
            "跨右/下边界的矩形必须被钳到页内"
        );
        assert_eq!(crossing.byte_count(512, 1), 144);
        // 打包必须逐字节来自页内的钳位区域；漏掉上界钳制会让这里的切片越界 panic。
        let mut page = vec![0u8; 512 * 512];
        for y in 500..512usize {
            for x in 500..512usize {
                page[y * 512 + x] = 0xA5;
            }
        }
        let mut out = Vec::new();
        pack_rects(&page, 512, 1, &crossing, &mut out);
        assert_eq!(out.len(), 144, "跨页矩形的上传字节 = 钳位后的面积");
        assert!(out.iter().all(|byte| *byte == 0xA5));

        // 只跨右边界（y 完全在页内）：单独锁住 x1 的上界钳制（M10 变异的目标）。
        let mut right_edge = DirtyRects::default();
        right_edge.mark(PhysicalRect::new(500, 10, 700, 14));
        assert_eq!(
            right_edge.chunks(512, 1).collect::<Vec<_>>(),
            vec![RectChunk {
                x: 500,
                y: 10,
                width: 12,
                height: 4,
                offset: 0,
            }]
        );
        assert_eq!(right_edge.byte_count(512, 1), 48);

        // 只跨下边界（x 完全在页内）：单独锁住 y1 的上界钳制。
        let mut bottom_edge = DirtyRects::default();
        bottom_edge.mark(PhysicalRect::new(10, 500, 14, 700));
        assert_eq!(
            bottom_edge.chunks(512, 1).collect::<Vec<_>>(),
            vec![RectChunk {
                x: 10,
                y: 500,
                width: 4,
                height: 12,
                offset: 0,
            }]
        );
        assert_eq!(bottom_edge.byte_count(512, 1), 48);
    }

    #[test]
    fn deallocate_is_available_outside_tests() {
        // F-147 的回归面：`deallocate` 必须是生产代码（`ResourceCache::unload` 的唯一调用点）。
        let mut atlas = allocator();
        let allocation = atlas.allocate((8, 8)).expect("fits");
        atlas.deallocate(allocation.id);
        assert!(atlas.allocate((8, 8)).is_some(), "释放后容量必须回来");
    }

    /// D-9：多页图片图集——单页放满后必须开第 2 页，`tile.page` 指向正确页。
    #[test]
    fn image_pager_opens_a_second_page_when_the_first_is_full() {
        // 64² 页 + 18×18 内容（20×20 分配）⇒ 每页 9 块左右，几轮就满。
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let mut page_0 = 0usize;
        let mut allocations = Vec::new();
        for _ in 0..32 {
            let Some(allocation) = pager.allocate((18, 18)) else {
                break;
            };
            if allocation.tile.page == 0 {
                page_0 += 1;
            }
            allocations.push(allocation);
        }
        assert!(page_0 > 0, "第一页必须被使用");
        assert!(
            allocations
                .iter()
                .any(|allocation| allocation.tile.page == 1),
            "单页放满后必须开第 2 页（tile.page 才有 1）"
        );
        assert!(pager.page_count() >= 2, "必须开过第 2 页");
    }

    /// D-9：页数达到 `MAX_IMAGE_PAGES` 且全部页满 ⇒ `None`（上层报 Capacity{PageFull}）。
    #[test]
    fn image_pager_reports_capacity_only_after_all_pages_are_full() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let mut allocations = Vec::new();
        while let Some(allocation) = pager.allocate((18, 18)) {
            allocations.push(allocation);
        }
        assert_eq!(
            pager.page_count(),
            usize::from(MAX_IMAGE_PAGES),
            "必须先把所有页填满才报容量失败"
        );
        let mut seen = [false; MAX_IMAGE_PAGES as usize];
        for allocation in &allocations {
            seen[usize::from(allocation.tile.page)] = true;
        }
        assert!(seen.iter().all(|used| *used), "每个页号都必须被用到");
    }

    /// D-9/RD-3：**只**回收已空的页，活 tile 永不淘汰；回收按 LRU（最久未用的空页先复用）。
    ///
    /// 必须钉死顺序（旧测试只断言 `recycled == 1 || 2`，把 `min_by_key` 改成 `max_by_key`
    /// 也能通过）：62² 内容 + 1 纹素 padding = 64² 整页（每页恰好一块），页 1 先变空
    /// （更久未用）⇒ 下一次分配必须落在页 1；页 2 其次。
    #[test]
    fn image_pager_recycles_the_least_recently_used_empty_page() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let mut per_page: ahash::HashMap<u16, Vec<etagere::AllocId>> = ahash::HashMap::default();
        while let Some(allocation) = pager.allocate((62, 62)) {
            per_page
                .entry(allocation.tile.page)
                .or_default()
                .push(allocation.id);
        }
        assert_eq!(pager.page_count(), usize::from(MAX_IMAGE_PAGES));
        // 页 1 先清空、页 2 后清空 ⇒ 页 1 是更久未用的空页。
        for id in per_page.remove(&1).expect("page 1 has tiles") {
            pager.deallocate(1, id);
        }
        for id in per_page.remove(&2).expect("page 2 has tiles") {
            pager.deallocate(2, id);
        }
        let first = pager.allocate((62, 62)).expect("存在空页时必须能分配");
        assert_eq!(
            first.tile.page, 1,
            "必须精确回收**最久未用**的空页（页 1），而不是任意空页"
        );
        let second = pager.allocate((62, 62)).expect("第二个空页");
        assert_eq!(second.tile.page, 2, "其次回收较近变空的页 2");
        assert_eq!(
            pager.page_count(),
            usize::from(MAX_IMAGE_PAGES),
            "回收空页不得增加页数"
        );
        // 使用中保护：页 0/3 仍有活 tile、且此时没有空页 ⇒ 分配必须失败（4 页全满），
        // 绝不为了腾空间回收活 tile。
        assert!(
            pager.allocate((62, 62)).is_none(),
            "活 tile 的页（0/3）绝不能被回收，且不得开第 5 页"
        );
    }

    /// 层数单调不减（P2-B3）：空闲页被 LRU **原地**回收后 `page_count()`（= 三端 texture array
    /// 的层数）不变小，回收页上重新分配的 tile 层号仍落在数组内。
    ///
    /// 变异：`page_count()` 改成「非空闲页数」⇒ 回收后页数变小 ⇒ 本测试失败（生产上会缩容
    /// 数组，让所有窗口已有 `AtlasTile.page` 指向不存在的层）。
    #[test]
    fn the_image_array_layer_count_never_shrinks_when_an_idle_page_is_recycled() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let first = pager.allocate((62, 62)).expect("page 0 tile");
        let second = pager.allocate((62, 62)).expect("page 1 tile");
        assert_eq!(pager.page_count(), 2, "两块 62² tile 各占满一页");
        assert_ne!(first.tile.page, second.tile.page);

        // 页变空 ⇒ 进入空闲表（LRU 候选）。
        pager.deallocate(first.tile.page, first.id);
        assert_eq!(
            pager.page_count(),
            2,
            "页变空不得减少页数（数组层数只增不减）"
        );

        // 下一次分配原地回收这个空页：页号稳定、层数不变、层号仍在数组范围内。
        let recycled = pager.allocate((62, 62)).expect("空页可原地回收");
        assert_eq!(
            recycled.tile.page, first.tile.page,
            "回收必须复用同一个页号（旧 tile 的 page 语义不受影响）"
        );
        assert_eq!(pager.page_count(), 2, "回收复用不得改变数组层数");
        assert!(
            u32::from(recycled.tile.page) < pager.page_count() as u32,
            "回收后分配的层号必须仍在数组范围内"
        );
    }

    /// D-9：新页第一次取脏矩形必须是整页哨兵（三端首次必须整层上传）。
    #[test]
    fn image_pager_new_page_is_fully_dirty() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let _ = pager.allocate((18, 18)).expect("fits");
        let dirty = pager.take_dirty_rects(0).expect("page 0 exists");
        assert!(dirty.is_full(), "新页首次上传必须是整页");
        assert!(pager.take_dirty_rects(1).is_none(), "第 2 页还没开");
        // 页 1 打开后同样整页脏。
        loop {
            if pager.allocate((18, 18)).is_none() {
                break;
            }
            if pager.page_count() == 2 {
                break;
            }
        }
        assert!(pager.page_count() >= 2, "必须开过第 2 页");
        assert!(
            pager.take_dirty_rects(1).expect("page 1 exists").is_full(),
            "新开的第 2 页首次上传必须是整页"
        );
    }

    /// D-9：页回收复用后必须换代（旧 tile 的 generation 不得与新一代混淆），且页号保持稳定。
    #[test]
    fn image_pager_recycled_page_bumps_its_generation() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let mut per_page: ahash::HashMap<u16, Vec<etagere::AllocId>> = ahash::HashMap::default();
        while let Some(allocation) = pager.allocate((18, 18)) {
            per_page
                .entry(allocation.tile.page)
                .or_default()
                .push(allocation.id);
        }
        let generation_before = pager.generation(0).expect("page 0");
        for id in per_page.remove(&0).expect("page 0 has tiles") {
            pager.deallocate(0, id);
        }
        let mut recycled = None;
        for _ in 0..64 {
            let Some(allocation) = pager.allocate((18, 18)) else {
                break;
            };
            if allocation.tile.page == 0 {
                recycled = Some(allocation);
                break;
            }
        }
        let recycled = recycled.expect("page 0 must be recycled (it is the only empty page)");
        assert_eq!(recycled.tile.page, 0, "回收复用必须保持页号稳定");
        assert_ne!(
            pager.generation(0).expect("page 0"),
            generation_before,
            "回收复用必须换代（旧 tile 不得与新 tile 共用 generation）"
        );
    }

    /// D-9：陈旧 `AllocId` 的重复释放不得把活 tile 计成 0（幂等）。
    #[test]
    fn image_pager_deallocate_is_idempotent() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let allocation = pager.allocate((18, 18)).expect("fits");
        pager.deallocate(0, allocation.id);
        pager.deallocate(0, allocation.id);
        pager.deallocate(99, allocation.id);
        // 页已空：下一次分配应能复用页 0（若幂等失败，活计数为负/丢页会表现成开新页或错误）。
        let next = pager.allocate((18, 18)).expect("fits");
        assert_eq!(next.tile.page, 0);
    }
}
