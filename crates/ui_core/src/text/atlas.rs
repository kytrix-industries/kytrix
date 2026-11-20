//! 字形 `key → AtlasTile` 缓存（P05 §10）：保证每个字形只光栅化一次。
//!
//! 与 [`crate::render::atlas::AtlasAllocator`] 的分工：本模块**做缓存 + 驱动光栅**，分配器只做装箱 +
//! 脏矩形 + 代际。图集 `clear()` 后必须同步 [`GlyphAtlas::clear`]（两级缓存同事务换代）。
//!
//! # 容量策略：逐 tile LRU，不丢帧（R12-1，对齐 Skia `SkStrikeCache` / WebRender `GlyphCache`）
//!
//! 旧实现只有两条出路：单页分配成功，或「已到 2048² ⇒ **清空全部** ⇒ 同一 pass 内再填满 ⇒
//! 再清空」，后者在两轮换代后返回 `EngineError::TextShape` ⇒ 整帧被丢弃 ⇒ 文本永久停止刷新。
//! 现在按三级降级，**任何一级都不产生 `TextShape` / 不丢帧**：
//!
//! 1. **阶梯扩容**：512 → 1024 → 2048（保留原有行为，换代递增 `generation`，调用方重跑 pass）；
//! 2. **逐 tile LRU 淘汰**：到 2048² 上限后按 `last_used_pass` 升序释放最旧的 tile 并重试。
//!    **本 pass**（= 本窗口本帧的一次 `App::layout_text`）用过的 tile 不淘汰，因此淘汰不会破坏
//!    本 pass 已生成的 `ShapedLine`。注意 `pass` 是**逐窗口**递增的：多窗口下同一帧里后跑的
//!    窗口 pass 号更大，可以淘汰先跑窗口本帧刚用过的 tile —— 「tile 只在本 pass 内有效」就是
//!    这个代价。`App::layout_text` 在淘汰发生时重跑本 pass，并让**其它窗口**在自己的下一帧
//!    重排（见 `app/text_pass.rs` 的 `any_eviction` 收尾与 `repaint_text_if_atlas_changed`）；
//! 3. **软失败**：整个页都是本 pass 的工作集 ⇒ 该字形**本帧不画**，但**不再静默**：
//!    `GlyphAtlasStats::skipped` 上抛 `TextLayoutReport::skipped_glyphs`，每个节点每轮压一条
//!    `TextDiagnostic::AtlasFull`，该节点的 `ShapedLine` 带「待重排」标记 ⇒ 下一帧必然重光栅
//!    重试。连续 [`ATLAS_FULL_ESCALATION_FRAMES`] 个 pass 仍有软失败，则升级为
//!    `EngineError::AtlasExhausted`（`AtlasKind::Glyph`）—— 可见失败优于静默缺字；计数随即
//!    清零，避免容量长期不足时每帧都报错。
//!
//! 逐 tile 释放不递增 `generation`（只由扩容/整页重建递增）：`generation` 的语义是「页尺寸/
//! 整页内容换代」，与单个 tile 的生死无关。被淘汰 tile 的矩形由**分配器的脏行**覆盖
//! （`allocate` 会 `mark_written`），所以上传路径不需要额外处理。
//!
//! # 淘汰纪元 `eviction_epoch`（v2.1 E1b，§11.1）
//!
//! 逐 tile LRU 淘汰**不**换代，却会让旧 `ShapedLine` / `Canvas` 里烘焙的 `AtlasTile` 指向
//! 已被别的字形复用的矩形（静默花字）。`generation` 无法表达这件事，因此本模块另维护一个
//! **累计**的 `eviction_epoch`：每淘汰一个 tile 就 +1，只增不减。`ShapedTextHandle` 同时记录
//! `(generation, eviction_epoch)`，任一变化都判定失效 —— 句柄因此不会把被复用的 UV 编码上屏。
//!
//! 纪元是**进程级**的（图集是进程级共享），与 `generation` 同一真源：`App::sync_atlas_generation_lease`
//! 把两者打包写进租约。`Canvas` 内已编码的图元不受句柄保护（push 之后发生的淘汰无法从
//! `AtlasTile` 反推），应用义务见 E1 规格：图集换代/容量压力后重新 `shape_text` 并重建 Canvas。

use super::raster::{FontKey, GlyphRasterizer};
use crate::{
    error::EngineError,
    paint::atlas::{AtlasKind, AtlasTile},
    render::{Allocation, AtlasAllocator},
};

/// 字形缓存键：字体身份 + 字形 id + **量化 ppem** + 水平亚像素相位。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GlyphKey {
    font: FontKey,
    glyph: u32,
    /// 光栅 ppem 的量化键（[`PPEM_KEY_QUANTIZATION`]）。光栅用的 ppem 是小数物理字号，
    /// 但缓存键只保留 1/64 px 的量化值：既让小数 ppem 参与键，又不对 f32 的 1 ulp 抖动敏感。
    ppem_q: u32,
    /// 水平亚像素相位桶（`0..raster::SUBPIXEL_PHASES`）：同一字形在同一 ppem 下每个相位各占
    /// 一个 tile，绘制时按 `整数基准 + placement.left` 落点（见 `raster::split_subpixel_x`）。
    subpixel_x: u8,
    /// 实例坐标 / faux bold 的摘要（见 `GlyphVariation`）：同一字形的不同实例各占一个 tile。
    variation: u64,
}

/// `GlyphKey` 的 ppem 量化步长：`ppem_q = (ppem * PPEM_KEY_QUANTIZATION).round()`。
///
/// 光栅用的 ppem 是**小数**物理字号（parley 的 `Run::font_size()` = 逻辑字号 × `scale`；
/// 16 px @125% → 20.0 ppem、14 px @125% → 17.5 ppem，见 `shape::ShapedText::rasterize`），
/// 但缓存键不能直接存 f32 bits：同一物理字号从不同路径重算 `size * scale` 可能差 1 ulp，
/// 直接比 bits 会让图集为视觉上完全相同的 ppem 多存一份 tile，`missing` 负缓存也会永远命不中。
/// 量化到 1/64 px 后，125%/150% DPI 下最常见的 17.5 / 18.75 等值是**精确**整数键（×64 无
/// 舍入），量化误差 ≤ 1/128 px —— 只有亚像素相位（1/4 px）的 1/32。
pub(crate) const PPEM_KEY_QUANTIZATION: f32 = 64.0;

/// 光栅 ppem → 可哈希的量化键（[`PPEM_KEY_QUANTIZATION`]）。
///
/// 非法值（NaN/±inf/≤0）在 `GlyphRasterizer::rasterize` 里被拒绝，`.round() as u32` 是饱和
/// 转换（NaN/负数归一为 0），不会 panic；生产路径的 ppem 已被 `clamp(1.0, 512.0)` 钳过，
/// 键 ∈ `64..=32768`，与非法值的键不可能相撞。
fn ppem_key(ppem: f32) -> u32 {
    (ppem * PPEM_KEY_QUANTIZATION).round() as u32
}

/// 一条缓存记录：tile + swash 的落点（位置要用 `top/left` 才算得出）。
struct GlyphRecord {
    tile: AtlasTile,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    /// 内容区在页面里的左上角（P09 整页上传的写入原点）。
    origin: (u32, u32),
    /// R8 覆盖度（**已加 padding 的内容区**，P09 的上传源；N2）。
    data: Box<[u8]>,
    /// 分配器的句柄（逐 tile 淘汰的唯一释放键）。
    alloc: etagere::AllocId,
    /// 该字形来自彩色源（R12-8；排版侧据此记 `ColorGlyphMonochrome` 诊断）。
    color: bool,
    /// 最近一次被使用的 pass 号（LRU 依据；`begin_pass` 递增）。
    last_used_pass: u32,
}

/// `ensure` 的返回值：tile 与落点。
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphTile {
    pub tile: AtlasTile,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// 该字形来自彩色源（R12-8）：排版侧据此记一条 `ColorGlyphMonochrome` 诊断。
    pub color: bool,
}

/// 缓存统计（`App::layout_text` 按节点累加进 `TextLayoutReport`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GlyphAtlasStats {
    pub hits: usize,
    pub misses: usize,
    pub rasters: usize,
    pub rebuilds: usize,
    /// 逐 tile LRU 淘汰的字形数（R12-1）。
    pub evictions: usize,
    /// 因「整页都是本帧工作集」而跳过的字形数（软失败，下一帧重试；**不进** `missing` 负缓存）。
    pub skipped: usize,
}

/// 连续多少个文本 pass 仍以「软失败」收尾后，升级为可见错误
/// [`EngineError::AtlasExhausted`]（`AtlasKind::Glyph`）。
///
/// 3 ≈ 3 帧：一次性抖动（工作集瞬时变大 / 前一帧刚淘汰）能自行恢复；连续 3 个 pass 都放不下
/// 说明容量确实不够 —— 此时静默缺字不可接受（R12-1 的「可见失败优于静默」），宁可让调用方
/// 丢弃那一帧并上报。判据由 [`GlyphAtlas::note_soft_fail`] 维护。
pub(crate) const ATLAS_FULL_ESCALATION_FRAMES: u32 = 3;

pub(crate) struct GlyphAtlas {
    records: ahash::HashMap<GlyphKey, GlyphRecord>,
    /// 空白/无轮廓/彩色字形的**负缓存**（N5 + R12-7：不再每帧重光栅空格与零尺寸字形）。
    missing: ahash::HashSet<GlyphKey>,
    stats: GlyphAtlasStats,
    /// 整张字形页被清空过（DPI 换代 / 容量逃逸重排）⇒ 旧代际的 tile 全部失效，
    /// 上传侧必须把整页内容重写到 GPU（padding 里可能残留旧字形的覆盖度）。
    cleared: bool,
    /// 当前 pass 号（每次 `App::layout_text` 递增；LRU 淘汰与「本帧不可淘汰」判据）。
    pass: u32,
    /// 自上次 [`GlyphAtlas::take_evicted`] 以来被淘汰的 tile 数。
    evicted: u32,
    /// **累计**淘汰纪元（只增不减）：每淘汰一个 tile +1。
    ///
    /// `generation` 只表达「整页换代」；逐 tile LRU 淘汰不换代，却让旧 `AtlasTile` 失效。
    /// 句柄的租约因此同时携带 `(generation, eviction_epoch)`（见模块文档）。
    eviction_epoch: u32,
    /// 连续多少个文本 pass 以「软失败」收尾（[`GlyphAtlas::note_soft_fail`] 维护；一次
    /// `skipped == 0` 的 pass 清零，达到 [`ATLAS_FULL_ESCALATION_FRAMES`] 时抛出错误并清零）。
    consecutive_soft_fail_passes: u32,
}

impl GlyphAtlas {
    pub(crate) fn new() -> Self {
        Self {
            records: ahash::HashMap::default(),
            missing: ahash::HashSet::default(),
            stats: GlyphAtlasStats::default(),
            cleared: false,
            pass: 0,
            evicted: 0,
            eviction_epoch: 0,
            consecutive_soft_fail_passes: 0,
        }
    }

    /// 开始一次文本 pass：本 pass 用到的 tile 记 `last_used_pass = pass`，且在本 pass 内不可淘汰。
    ///
    /// `pass` 由**每个窗口每帧**的一次 `App::layout_text` 递增（不是进程级帧号）：同一帧里后跑
    /// 的窗口 pass 号更大，可以淘汰先跑窗口本 pass 用过的 tile —— 这是「tile 只在本 pass 内
    /// 有效」的语义，见模块文档第 2 级降级。
    pub(crate) fn begin_pass(&mut self) {
        self.pass = self.pass.wrapping_add(1);
    }

    /// 一次文本 pass 结束后的软失败记账（R12-1：可见失败优于静默缺字）。
    ///
    /// `skipped` = 本 pass **最后一轮重跑**仍没画出的字形数（`0` = 该放下的都放下了）。
    /// 连续 [`ATLAS_FULL_ESCALATION_FRAMES`] 个 pass 都非零 ⇒ 返回
    /// [`EngineError::AtlasExhausted`] 并把计数清零：容量真的不够时让失败可见，但不是此后
    /// 每帧都报错（调用方丢弃那一帧，下一帧仍会尝试恢复）。
    ///
    /// 计数放在图集上而不是 `App` 上：图集是**进程级**共享的，软失败也是图集级状态，而
    /// `GlyphAtlas` 不持有窗口身份。因此任一窗口的一次干净 pass 都会清零 —— 那一次 pass 确实
    /// 把该放下的字形都放下了。
    pub(crate) fn note_soft_fail(&mut self, skipped: usize) -> Result<(), EngineError> {
        if skipped == 0 {
            self.consecutive_soft_fail_passes = 0;
            return Ok(());
        }
        self.consecutive_soft_fail_passes = self.consecutive_soft_fail_passes.saturating_add(1);
        if self.consecutive_soft_fail_passes >= ATLAS_FULL_ESCALATION_FRAMES {
            self.consecutive_soft_fail_passes = 0;
            return Err(EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph,
            });
        }
        Ok(())
    }

    /// 取走「自上次调用以来淘汰了多少 tile」（淘汰会让旧 `ShapedLine` 的 UV 失效，
    /// `App::layout_text` 据此重跑 pass 并让其它窗口重排）。
    pub(crate) fn take_evicted(&mut self) -> u32 {
        core::mem::take(&mut self.evicted)
    }

    /// **累计**淘汰纪元（只增不减；每淘汰一个 tile +1，与 `generation` 一起构成句柄租约）。
    pub(crate) fn eviction_epoch(&self) -> u32 {
        self.eviction_epoch
    }

    /// 取字形 tile；未命中则光栅 + 装箱。
    ///
    /// `ppem` 是**小数**物理字号：光栅按原值进行（`GlyphRasterizer::rasterize` 收 `f32`，
    /// swash/skrifa 直接按小数缩放轮廓），缓存键只用它的量化值 [`PPEM_KEY_QUANTIZATION`]。
    /// 相差 < 1/128 px 的两个 ppem 因此共用一份位图 —— 尺寸差不可见，却避免了同一物理字号
    /// 因 1 ulp 抖动重复光栅。
    ///
    /// `subpixel_x` 是水平相位桶（`0..raster::SUBPIXEL_PHASES`），参与缓存键：同一字形的不同
    /// 相位必须各占一个 tile，否则亚像素定位会被 tile 复用抹平。
    ///
    /// **永不失败**（R12-1）：最坏情况是返回 `Ok(None)`（本帧跳过该字形）。两种 `None` 语义
    /// 不同：字体确实没有该字形 ⇒ 写 `missing` 负缓存、之后永久跳过；图集整页都是本帧工作集
    /// ⇒ `stats.skipped += 1`、**不写**负缓存，由 `App::layout_text` 上抛统计 + 诊断 + 重排，
    /// 下一帧重试。
    pub(crate) fn ensure(
        &mut self,
        raster: &mut GlyphRasterizer,
        atlas: &mut AtlasAllocator,
        font: &parley::FontData,
        glyph: u32,
        ppem: f32,
        subpixel_x: u8,
        variation: Option<&crate::text::raster::GlyphVariation>,
    ) -> Result<Option<GlyphTile>, EngineError> {
        let key = GlyphKey {
            font: FontKey::of(font, raster.font_epoch()),
            glyph,
            ppem_q: ppem_key(ppem),
            subpixel_x,
            variation: variation.map_or(0, crate::text::raster::GlyphVariation::hash),
        };
        if self.missing.contains(&key) {
            return Ok(None);
        }
        if let Some(found) = self.records.get_mut(&key)
            && !found.tile.is_stale(atlas.generation())
        {
            found.last_used_pass = self.pass;
            let record = &*found;
            self.stats.hits += 1;
            return Ok(Some(GlyphTile {
                tile: record.tile,
                left: record.left,
                top: record.top,
                width: record.width,
                height: record.height,
                color: record.color,
            }));
        }
        self.stats.misses += 1;

        let Some(image) = raster.rasterize(
            font,
            glyph,
            ppem,
            subpixel_x,
            variation.unwrap_or(&crate::text::raster::GlyphVariation::default()),
        ) else {
            // R12-8：彩色字形（COLR/CBDT/sbix）在光栅层已经按 alpha 投影成 R8 覆盖度；
            // 连轮廓都没有的字形（真空白）才进负缓存。
            self.missing.insert(key);
            return Ok(None);
        };
        let (width, height) = (image.placement.width, image.placement.height);
        debug_assert_eq!(
            image.data.len() as u32,
            width * height,
            "R8 alpha must be width * height bytes"
        );
        if width == 0 || height == 0 {
            // R12-7：空格等零尺寸字形必须进负缓存，否则每个 pass 都重光栅一遍。
            self.missing.insert(key);
            return Ok(None);
        }
        self.stats.rasters += 1;
        let content = (width, height);
        let allocation = self.allocate_with_eviction(atlas, content);
        let Some(allocation) = allocation else {
            // 3) 软失败：整页都是本帧工作集 ⇒ 本帧少画这个字形（**不进**负缓存，下一帧重试）。
            //    绝不返回错误：图集容量压力不该让整帧失效（R12-1 的硬要求）；可观测性与恢复
            //    由 `stats.skipped` → `TextLayoutReport::skipped_glyphs` / `AtlasFull` 诊断 /
            //    待重排标记，以及连续帧的 `EngineError::AtlasExhausted` 升级承担（见模块文档）。
            self.stats.skipped += 1;
            return Ok(None);
        };
        // 读全 `Allocation` 的四个字段（P02 §10.4 的契约；P09 的上传目标是 `content`）。
        debug_assert_eq!(allocation.tile.atlas, AtlasKind::Glyph);
        debug_assert_eq!(allocation.content.width() as u32, width);
        debug_assert_eq!(allocation.content.height() as u32, height);
        debug_assert!(allocation.padded.width() >= allocation.content.width());
        let record = GlyphRecord {
            tile: allocation.tile,
            left: image.placement.left,
            top: image.placement.top,
            width,
            height,
            origin: (
                allocation.content.x0.max(0) as u32,
                allocation.content.y0.max(0) as u32,
            ),
            data: image.data.into_boxed_slice(),
            color: image.color,
            alloc: allocation.id,
            last_used_pass: self.pass,
        };
        let tile = GlyphTile {
            tile: record.tile,
            left: record.left,
            top: record.top,
            width: record.width,
            height: record.height,
            color: record.color,
        };
        self.records.insert(key, record);
        debug_assert_eq!(
            self.coverage(key).map_or(0, |data| data.len()) as u32,
            width * height,
            "coverage must be retrievable after insert (P09 upload source)"
        );
        Ok(Some(tile))
    }

    /// 三级降级分配（R12-1）：直接分配 → 阶梯扩容 → 逐 tile LRU 淘汰。`None` = 本帧放不下。
    ///
    /// 淘汰只针对 `last_used_pass < self.pass` 的 tile：本帧用过的 tile 一旦被复用，本 pass
    /// 早先生成的 `ShapedLine` 就会画到别人的位图。淘汰是**批量**的（一次至少释放
    /// `records.len() / 8` 个），避免「每来一个字形就重新排序一次」。
    fn allocate_with_eviction(
        &mut self,
        atlas: &mut AtlasAllocator,
        content: (u32, u32),
    ) -> Option<Allocation> {
        if let Some(allocation) = atlas.allocate(content) {
            return Some(allocation);
        }
        // 1) 阶梯扩容（512 → 1024 → 2048）：整页重建 ⇒ 记录必须同步清空（同事务换代）。
        let mut grown = false;
        while atlas.grow() {
            grown = true;
            if let Some(allocation) = atlas.allocate(content) {
                self.stats.rebuilds += 1;
                self.clear();
                return Some(allocation);
            }
        }
        if grown {
            self.stats.rebuilds += 1;
            self.clear();
        }
        // 2) 已到上限：逐 tile LRU 淘汰最旧的、**非本帧**的 tile，直到放得下。
        loop {
            let mut candidates: Vec<(u32, GlyphKey)> = self
                .records
                .iter()
                .filter(|(_, record)| record.last_used_pass < self.pass)
                .map(|(key, record)| (record.last_used_pass, *key))
                .collect();
            if candidates.is_empty() {
                return None;
            }
            candidates.sort_unstable_by_key(|(last_used, _)| *last_used);
            let batch = (candidates.len() / 8).max(1);
            for (_, key) in candidates.into_iter().take(batch) {
                if let Some(record) = self.records.remove(&key) {
                    atlas.deallocate(record.alloc);
                    self.evicted = self.evicted.wrapping_add(1);
                    self.eviction_epoch = self.eviction_epoch.wrapping_add(1);
                    self.stats.evictions += 1;
                }
            }
            if let Some(allocation) = atlas.allocate(content) {
                return Some(allocation);
            }
        }
    }

    /// 与 `AtlasAllocator::clear()` 同一事务调用。
    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.missing.clear();
        // 换代之后整页内容都要重传（旧代际的 tile 全失效）。
        self.cleared = true;
    }

    /// 取走「页面被清空过」的信号：上传侧据此把整页内容重写到每个窗口的纹理。
    pub(crate) fn take_cleared(&mut self) -> bool {
        core::mem::take(&mut self.cleared)
    }

    /// 把所有 tile 的覆盖度写进整页缓冲区（`R8`、行主序、页宽 `page_width`）。
    ///
    /// 只在「整页要重传」的帧调用（新窗口 / 页尺寸变化 / 图集清空）；增量帧走
    /// [`GlyphAtlas::write_rows`]，只碰脏矩形覆盖到的字形（DEVIATIONS F-211 / R06-3）。
    pub(crate) fn write_page(&self, page: &mut [u8], page_width: u32) {
        for record in self.records.values() {
            write_record_rows(page, page_width, record, 0, record.height);
        }
    }

    /// 只把 `rects` 覆盖到的字形行写进整页缓冲区（增量上传的 CPU 侧）。
    ///
    /// 与 [`GlyphAtlas::write_page`] 的区别：不重写整张页，代价随**脏矩形数**而不是字形总数
    /// 增长（2000 个字形的窗口打一个字不再整页重写）。每个字形记录按自身 x 区间落位，因此
    /// 只需要判断「记录的 y 区间是否与脏矩形的 y 区间相交」：相交就重写该行的完整记录内容
    /// （幂等，且不外扩脏矩形）。
    pub(crate) fn write_rows(
        &self,
        page: &mut [u8],
        page_width: u32,
        rects: &crate::render::DirtyRects,
    ) {
        for record in self.records.values() {
            let record_y0 = record.origin.1;
            let record_y1 = record_y0 + record.height;
            for rect in rects.rects() {
                let start = rect.y0.max(record_y0 as i32).min(record_y1 as i32);
                let end = rect.y1.max(record_y0 as i32).min(record_y1 as i32);
                if start < end {
                    write_record_rows(
                        page,
                        page_width,
                        record,
                        (start - record_y0 as i32) as u32,
                        (end - record_y0 as i32) as u32,
                    );
                }
            }
        }
    }

    /// 字形覆盖度（P09 的上传源；`None` = 空白/未光栅）。
    pub(crate) fn coverage(&self, key: GlyphKey) -> Option<&[u8]> {
        self.records.get(&key).map(|record| record.data.as_ref())
    }

    /// 取走统计（`App::layout_text` 按节点累加进 `TextLayoutReport` 后清零）。
    ///
    /// **单一消费者**语义：光栅只发生在第二阶段（`layout_text`），第一阶段（`refresh_text`）
    /// 取到的永远是空统计。
    pub(crate) fn take_stats(&mut self) -> GlyphAtlasStats {
        core::mem::take(&mut self.stats)
    }

    /// 仅测试：缓存条目数。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    /// 仅测试：负缓存条目数。
    #[cfg(test)]
    pub(crate) fn missing_len(&self) -> usize {
        self.missing.len()
    }
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new()
    }
}

/// 把一条字形记录的内容区第 `row0..row1` 行写进整页缓冲。
fn write_record_rows(page: &mut [u8], page_width: u32, record: &GlyphRecord, row0: u32, row1: u32) {
    let (x0, y0) = record.origin;
    let row_len = record.width as usize;
    let pitch = page_width as usize;
    for row in row0..row1 {
        let destination = (y0 + row) as usize * pitch + x0 as usize;
        let source = row as usize * row_len;
        if destination + row_len > page.len() || source + row_len > record.data.len() {
            continue;
        }
        page[destination..destination + row_len]
            .copy_from_slice(&record.data[source..source + row_len]);
    }
}

#[cfg(test)]
mod tests {
    use super::{ATLAS_FULL_ESCALATION_FRAMES, GlyphAtlas, GlyphAtlasStats, GlyphKey, ppem_key};
    use crate::{
        paint::atlas::AtlasKind,
        render::AtlasAllocator,
        text::raster::{GlyphRasterizer, probe_font},
    };

    /// 空格（U+0020）的 glyph id（字体缺该字形时 `None`）。
    ///
    /// 空格是「有字形、但轮廓为空 ⇒ 0×0 位图」的真实路径，正是 R12-7 的回归面。
    fn space_glyph(font: &parley::FontData) -> Option<u32> {
        let font_ref = swash::FontRef::from_index(font.data.data(), font.index as usize)?;
        let id = font_ref.charmap().map(' ');
        (id != 0).then_some(u32::from(id))
    }

    fn tiny_glyph_atlas() -> AtlasAllocator {
        AtlasAllocator::with_page(AtlasKind::Glyph, (256, 256))
    }

    #[test]
    fn glyph_tile_is_cached_per_key() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();

        let first = glyphs.ensure(&mut raster, &mut atlas, &font, 36, 16.0, 0, None);
        let second = glyphs.ensure(&mut raster, &mut atlas, &font, 36, 16.0, 0, None);
        let first = first.expect("no error").expect("glyph has coverage");
        let second = second.expect("no error").expect("cached");

        assert_eq!(first.tile, second.tile);
        let stats = glyphs.take_stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.rasters, 1);
        assert_eq!(glyphs.len(), 1);
    }

    #[test]
    fn glyph_tile_differs_per_ppem() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();

        let small = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 13.0, 0, None)
            .expect("ok");
        let large = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 26.0, 0, None)
            .expect("ok");
        let (small, large) = (small.expect("13ppem"), large.expect("26ppem"));

        assert_ne!(
            small.tile.uv(),
            large.tile.uv(),
            "different ppem ⇒ different tile"
        );
        assert!(large.width > small.width);
    }

    /// P3（txt-1 收尾）：ppem 键必须是「小数 + 量化」，而不是整数 ppem 或裸 f32 bits。
    ///
    /// 125% DPI 下 14 px 逻辑字号 = 17.5 ppem：它必须与 18 ppem 分开（把 ppem 取整的实现会让
    /// 两者撞键、125% 下按 18 px 出位图），但 17.5 与 17.5+1e-5（同一物理字号的不同重算路径）
    /// 必须命中同一 tile（裸 f32 bits 的键会因 1 ulp 差异重复光栅）。
    #[test]
    fn fractional_ppem_is_quantized_in_the_glyph_key() {
        assert_eq!(ppem_key(17.5), 1120, "17.5 × 64 必须精确");
        assert_eq!(ppem_key(18.0), 1152);
        assert_eq!(ppem_key(16.0), 1024);
        assert_eq!(ppem_key(17.5 + 1e-5), 1120, "1/64 px 内的抖动必须同键");
        assert_eq!(ppem_key(f32::NAN), 0, "NaN 必须归一为固定键");
        assert_eq!(ppem_key(-1.0), 0);

        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();

        let half = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 17.5, 0, None)
            .expect("ok")
            .expect("17.5 ppem coverage");
        let eighteen = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 18.0, 0, None)
            .expect("ok")
            .expect("18 ppem coverage");
        assert_ne!(
            half.tile.uv(),
            eighteen.tile.uv(),
            "17.5 与 18 ppem 必须各占一个 tile（取整实现会撞键）"
        );

        let near = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 17.5 + 1e-5, 0, None)
            .expect("ok")
            .expect("quantized hit");
        assert_eq!(
            near.tile, half.tile,
            "1/64 px 内的同一物理字号必须复用 tile"
        );
        let stats = glyphs.take_stats();
        assert_eq!((stats.misses, stats.rasters, stats.hits), (2, 2, 1));
        assert_eq!(glyphs.len(), 2);
    }

    /// P2-2：水平亚像素相位必须参与缓存键 —— 否则 x=10.0 与 x=10.4 的字形会共用同一张位图，
    /// 亚像素定位被 tile 复用抹平。同相位必须继续复用（不能把缓存打成全 miss）。
    #[test]
    fn glyph_tile_differs_per_subpixel_phase() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();

        let integer = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, 0, None)
            .expect("ok")
            .expect("integer phase");
        let half = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, 2, None)
            .expect("ok")
            .expect("half phase");
        assert_ne!(
            integer.tile.uv(),
            half.tile.uv(),
            "1/4 px 相位桶必须各占一个 tile"
        );
        let stats = glyphs.take_stats();
        assert_eq!((stats.misses, stats.rasters, stats.hits), (2, 2, 0));
        assert_eq!(glyphs.len(), 2);

        let integer_again = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, 0, None)
            .expect("ok")
            .expect("cached integer phase");
        assert_eq!(integer.tile, integer_again.tile, "同相位必须复用 tile");
        let stats = glyphs.take_stats();
        assert_eq!((stats.misses, stats.rasters, stats.hits), (0, 0, 1));
    }

    /// P3（txt-1 收尾）：相位 3 → 0 的进位边界（x=10.999）必须与下一个整数完全同键。
    ///
    /// `split_subpixel_x(10.999)` 进位成 `(11.0, 0)`；图集侧必须复用相位 0 的 tile，不能把
    /// 10.999 与 11.0 当成两种位置（那等于凭空多出第 5 档相位/多一份位图）。
    #[test]
    fn phase_carry_reuses_the_integer_phase_tile() {
        let (base, phase) = crate::text::raster::split_subpixel_x(10.999);
        assert_eq!((base, phase), (11.0, 0), "10.999 必须进位成 x=11 + 相位 0");
        assert_eq!(
            crate::text::raster::split_subpixel_x(11.0),
            (base, phase),
            "进位后必须与整像素完全同键"
        );

        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();

        let integer = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, 0, None)
            .expect("ok")
            .expect("phase 0 coverage");
        let three_quarter = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, 3, None)
            .expect("ok")
            .expect("phase 3 coverage");
        assert_ne!(
            integer.tile.uv(),
            three_quarter.tile.uv(),
            "3/4 px 相位必须新建 tile"
        );
        assert_eq!(glyphs.len(), 2);

        let carried = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, phase, None)
            .expect("ok")
            .expect("carried phase must hit");
        assert_eq!(
            carried.tile, integer.tile,
            "进位到相位 0 必须复用整像素 tile"
        );
        assert_eq!(glyphs.len(), 2, "进位不得新建第 5 档 tile");
        let stats = glyphs.take_stats();
        assert_eq!((stats.misses, stats.rasters, stats.hits), (2, 2, 1));
    }

    /// P3（txt-1 收尾）：`missing` 负缓存键必须包含量化 ppem —— 否则同一无轮廓字形在 125%
    /// （17.5 ppem）首次失败后，100%（16 ppem）的失败结果会被错误复用（反过来也一样），
    /// 换成小数 ppem 后负缓存的失效维度就少了一维。
    #[test]
    fn missing_cache_is_keyed_by_quantized_ppem() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();
        // 非法 glyph id：swash 取不到轮廓，`rasterize` 返回 None（不依赖宿主字体）。
        let missing_glyph = u32::from(u16::MAX);

        assert!(
            glyphs
                .ensure(&mut raster, &mut atlas, &font, missing_glyph, 17.5, 0, None)
                .expect("no error")
                .is_none()
        );
        let stats = glyphs.take_stats();
        assert_eq!((stats.misses, stats.rasters), (1, 0));

        // 同键必须直接命中负缓存：不再尝试光栅、也不计 miss。
        assert!(
            glyphs
                .ensure(&mut raster, &mut atlas, &font, missing_glyph, 17.5, 0, None)
                .expect("no error")
                .is_none()
        );
        assert_eq!(glyphs.take_stats(), super::GlyphAtlasStats::default());

        // 量化 ppem 是负缓存键的一部分：18.0 必须重新尝试并再次 miss。
        assert!(
            glyphs
                .ensure(&mut raster, &mut atlas, &font, missing_glyph, 18.0, 0, None)
                .expect("no error")
                .is_none()
        );
        assert_eq!(glyphs.take_stats().misses, 1);
    }

    #[test]
    fn cleared_atlas_reallocs_and_old_tiles_go_stale() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();

        let first = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, 0, None)
            .expect("ok")
            .expect("coverage");
        atlas.clear();
        glyphs.clear();
        let second = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, 0, None)
            .expect("ok")
            .expect("coverage");

        assert!(first.tile.is_stale(atlas.generation()));
        assert!(!second.tile.is_stale(atlas.generation()));
        assert_eq!(second.tile.generation, atlas.generation());
    }

    #[test]
    fn font_epoch_is_part_of_the_glyph_key() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let first_key = GlyphKey {
            variation: 0,
            font: super::FontKey::of(&font, raster.font_epoch()),
            glyph: 36,
            ppem_q: ppem_key(16.0),
            subpixel_x: 0,
        };
        raster.set_font_epoch(raster.font_epoch() + 1);
        let second_key = GlyphKey {
            variation: 0,
            font: super::FontKey::of(&font, raster.font_epoch()),
            glyph: 36,
            ppem_q: ppem_key(16.0),
            subpixel_x: 0,
        };

        assert_ne!(first_key, second_key, "epoch bump must change the key");
    }

    /// F-208：起始页放不下时**先换代扩容**，不再直接清空同页重试。
    #[test]
    fn a_tiny_page_grows_to_the_next_ladder_step() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (1, 1));
        let mut glyphs = GlyphAtlas::new();
        let old_generation = atlas.generation();

        let tile = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0, 0, None)
            .expect("growth must rescue the glyph")
            .expect("glyph has coverage");

        assert_eq!(atlas.page(), (512, 512), "1×1 长到阶梯的第一档");
        assert_eq!(
            atlas.generation(),
            old_generation + 1,
            "换代只加一次（单次扩容就放得下）"
        );
        assert_eq!(tile.tile.generation, atlas.generation());
        let stats = glyphs.take_stats();
        assert_eq!(stats.rebuilds, 1);
        assert_eq!(stats.rasters, 1);
    }

    /// R12-7：零尺寸字形（空格）必须进负缓存 —— 第二次 `ensure` 不得再光栅、不得计 miss。
    ///
    /// 旧实现在 `width == 0 || height == 0` 分支直接返回，没写 `missing`，于是价格文本里的每个
    /// 空格在每个 pass 都重走一遍 swash 光栅（"123.45 USDT" 每条 1–3 个空格）。
    #[test]
    fn zero_size_glyphs_are_negatively_cached() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();
        // 空格（U+0020）的 glyph id：走真实路径（有字形但零轮廓）。
        let Some(space) = space_glyph(&font) else {
            eprintln!("skip: the probe font has no space glyph");
            return;
        };

        let first = glyphs
            .ensure(&mut raster, &mut atlas, &font, space, 16.0, 0, None)
            .expect("space must not fail");
        assert!(first.is_none(), "空格没有覆盖度");
        let stats = glyphs.take_stats();
        assert_eq!(
            (stats.misses, stats.rasters),
            (1, 0),
            "零尺寸字形不得计入 rasters"
        );
        assert_eq!(glyphs.missing_len(), 1, "必须写入负缓存");

        let second = glyphs
            .ensure(&mut raster, &mut atlas, &font, space, 16.0, 0, None)
            .expect("space must not fail");
        assert!(second.is_none());
        assert_eq!(
            glyphs.take_stats(),
            GlyphAtlasStats::default(),
            "第二次必须直接命中负缓存（不再光栅）"
        );
    }

    /// R12-1：图集到上限后**逐 tile LRU 淘汰**，永不失败；淘汰最旧的、且保留本 pass 用过的。
    ///
    /// 用 `cap_page_for_test` 把页档钉在 32²（生产是 2048²），这样十几个 tile 就能触发淘汰路径。
    #[test]
    fn lru_evicts_the_oldest_tiles_and_never_fails() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (32, 32));
        atlas.cap_page_for_test(32);
        let mut glyphs = GlyphAtlas::new();

        // pass 1：塞满一小页（16 ppem 的字形 padded 约 12×16 ⇒ 32² 只放得下几块）。
        glyphs.begin_pass();
        let mut live = Vec::new();
        for glyph_id in 36..64u32 {
            let tile = glyphs
                .ensure(&mut raster, &mut atlas, &font, glyph_id, 16.0, 0, None)
                .expect("capacity pressure must never surface as an error");
            if let Some(tile) = tile {
                live.push((glyph_id, tile));
            }
        }
        assert!(!live.is_empty(), "some glyphs must fit a 32² page");
        let filled = glyphs.len();

        // pass 2：先取一个本 pass 的 tile，再用新字形挤占 ⇒ 只能淘汰 pass 1 的。
        glyphs.begin_pass();
        let (protected_id, protected) = live[0];
        let again = glyphs
            .ensure(&mut raster, &mut atlas, &font, protected_id, 16.0, 0, None)
            .expect("ok")
            .expect("still cached");
        assert_eq!(protected.tile, again.tile, "命中缓存不换 tile");
        for index in 0..24u32 {
            let tile = glyphs
                .ensure(&mut raster, &mut atlas, &font, 200 + index, 16.0, 0, None)
                .expect("LRU eviction must keep `ensure` infallible");
            let _ = tile;
        }
        let stats = glyphs.take_stats();
        assert!(
            stats.evictions > 0 || glyphs.len() <= filled,
            "淘汰路径必须被走到（stats: {stats:?}, 之前 {filled} 条）"
        );
        assert!(
            stats.skipped == 0 || stats.evictions > 0,
            "skip 只能发生在淘汰之后（stats: {stats:?}）"
        );
        let protected_again = glyphs
            .ensure(&mut raster, &mut atlas, &font, protected_id, 16.0, 0, None)
            .expect("ok")
            .expect("the current-pass tile must survive");
        assert_eq!(
            protected.tile, protected_again.tile,
            "本 pass 用过的 tile 不得被淘汰（否则本帧早先的 ShapedLine 会画错）"
        );
    }

    /// P1-① / R12-1：容量不足时的软失败只是**本帧**跳过 —— 既不写 `missing` 负缓存（否则该
    /// 字形在之后所有 pass 都永久不画），也不妨碍下一个 pass 把它真正放进图集。
    ///
    /// 用 32² 的小页把「整页都是本 pass 工作集」这一支真实触发出来（`cap_page_for_test` 钉死
    /// 上限，`grow` 无处可扩）。
    #[test]
    fn a_full_page_soft_fails_but_never_poisons_the_glyph_cache() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (32, 32));
        atlas.cap_page_for_test(32);
        let mut glyphs = GlyphAtlas::new();

        // pass 1：把页塞满（16 ppem 的字形 padded 约 14×16 ⇒ 32² 只放得下几块），直到本 pass
        // 自己就再也放不下 —— 此时淘汰候选为空（所有 tile 都是本 pass 的）。
        glyphs.begin_pass();
        let mut blocked = None;
        for glyph_id in 36..96u32 {
            let _tile = glyphs
                .ensure(&mut raster, &mut atlas, &font, glyph_id, 16.0, 0, None)
                .expect("容量压力绝不能变成 `ensure` 的错误");
            let stats = glyphs.take_stats();
            if stats.skipped > 0 {
                blocked = Some((glyph_id, stats));
                break;
            }
        }
        let (blocked, stats) = blocked.expect("32² 的页必须被本 pass 的工作集占满");
        assert_eq!(stats.skipped, 1);
        assert_eq!(
            stats.rasters, 1,
            "软失败的字形必须先光栅（`skipped <= rasters` 的前提）"
        );

        // 同一 pass 再要一次：仍然 skip（本 pass 的 tile 不淘汰），而且**重新光栅** —— 这正是
        // 「没写进 missing 负缓存」的证据：负缓存命中时 misses/rasters 都会是 0。
        assert!(
            glyphs
                .ensure(&mut raster, &mut atlas, &font, blocked, 16.0, 0, None)
                .expect("no error")
                .is_none()
        );
        let again = glyphs.take_stats();
        assert_eq!(
            (again.misses, again.rasters, again.skipped),
            (1, 1, 1),
            "软失败不得进 `missing` 负缓存（stats: {again:?}）"
        );

        // pass 2：上一 pass 的 tile 变成可淘汰 ⇒ 同一个字形这次真的放得下。
        glyphs.begin_pass();
        let tile = glyphs
            .ensure(&mut raster, &mut atlas, &font, blocked, 16.0, 0, None)
            .expect("no error")
            .expect("下一 pass 必须补上这个字形（软失败不是永久污染）");
        let next = glyphs.take_stats();
        assert_eq!(next.skipped, 0, "淘汰腾位置后不再软失败");
        assert!(
            next.evictions > 0,
            "必须靠逐 tile LRU 淘汰腾位置（stats: {next:?}）"
        );
        assert_eq!(tile.tile.generation, atlas.generation());
        let key = GlyphKey {
            font: super::FontKey::of(&font, raster.font_epoch()),
            glyph: blocked,
            ppem_q: ppem_key(16.0),
            subpixel_x: 0,
            variation: 0,
        };
        assert!(
            glyphs.coverage(key).is_some(),
            "补上的字形必须真的进了缓存（可读回覆盖度）"
        );
        assert!(
            !glyphs.missing.contains(&key),
            "软失败的字形永远不能出现在负缓存里"
        );
    }

    /// E1b F1：逐 tile LRU 淘汰**不**换代（`generation` 不变），但必须前进累计淘汰纪元
    /// `eviction_epoch` —— `ShapedTextHandle` 的租约靠它感知「tile 已被别的字形复用」。
    #[test]
    fn lru_eviction_advances_the_eviction_epoch_without_bumping_the_generation() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (32, 32));
        atlas.cap_page_for_test(32);
        let mut glyphs = GlyphAtlas::new();
        let generation = atlas.generation();
        assert_eq!(glyphs.eviction_epoch(), 0);

        // pass 1：把 32² 页塞满。
        glyphs.begin_pass();
        for glyph_id in 36..48u32 {
            let _ = glyphs
                .ensure(&mut raster, &mut atlas, &font, glyph_id, 16.0, 0, None)
                .expect("容量压力绝不该变成错误");
        }
        assert_eq!(glyphs.eviction_epoch(), 0, "只填充不淘汰 ⇒ 纪元不动");

        // pass 2：新字形挤占 ⇒ 只能淘汰上一 pass 的 tile。
        glyphs.begin_pass();
        for glyph_id in 200..224u32 {
            let _ = glyphs
                .ensure(&mut raster, &mut atlas, &font, glyph_id, 16.0, 0, None)
                .expect("LRU 淘汰保证 ensure 永不失败");
        }
        assert!(glyphs.take_evicted() > 0, "必须真的发生过淘汰");
        assert_eq!(
            atlas.generation(),
            generation,
            "逐 tile 淘汰不换代（正是 epoch 存在的理由）"
        );
        assert!(glyphs.eviction_epoch() > 0, "每淘汰一个 tile 纪元 +1");
    }

    /// P1-① 的可见失败兜底：连续 [`ATLAS_FULL_ESCALATION_FRAMES`] 个 pass 都软失败才升级为
    /// [`EngineError::AtlasExhausted`]；中间有一次干净 pass（`skipped == 0`）就清零；升级后
    /// 计数也清零（避免容量长期不足时每帧都报错）。
    #[test]
    fn consecutive_soft_fail_passes_escalate_to_atlas_exhausted() {
        let mut glyphs = GlyphAtlas::new();
        for pass in 1..ATLAS_FULL_ESCALATION_FRAMES {
            assert!(
                glyphs.note_soft_fail(2).is_ok(),
                "第 {pass} 个连续软失败 pass 还没到阈值"
            );
        }
        // 一次干净 pass 必须清零：下一轮重新从 1 数。
        assert!(glyphs.note_soft_fail(0).is_ok());
        for _ in 1..ATLAS_FULL_ESCALATION_FRAMES {
            assert!(
                glyphs.note_soft_fail(1).is_ok(),
                "一次干净 pass 之后必须重新计数"
            );
        }
        let error = glyphs
            .note_soft_fail(1)
            .expect_err("连续软失败达到阈值必须升级为 EngineError::AtlasExhausted");
        assert!(matches!(
            error,
            crate::error::EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph
            }
        ));
        // 升级时清零：紧随其后的一个软失败 pass 不能立刻再报错。
        assert!(
            glyphs.note_soft_fail(1).is_ok(),
            "升级后必须清零，避免每帧都报错"
        );
    }
}
