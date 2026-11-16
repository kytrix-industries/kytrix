//! 字形 `key → AtlasTile` 缓存（P05 §10）：保证每个字形只光栅化一次。
//!
//! 与 [`crate::render::atlas::AtlasAllocator`] 的分工：本模块**做缓存 + 驱动光栅**，分配器只做装箱 +
//! 脏行 + 代际。图集 `clear()` 后必须同步 [`GlyphAtlas::clear`]（两级缓存同事务换代）。

use super::raster::{FontKey, GlyphRasterizer};
use crate::{
    error::EngineError,
    paint::atlas::{AtlasKind, AtlasTile},
    render::{Allocation, AtlasAllocator},
};

/// 字形缓存键：字体身份 + 字形 id + 物理 ppem + 水平亚像素相位。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GlyphKey {
    font: FontKey,
    glyph: u32,
    ppem: u32,
    /// 水平亚像素相位桶（`0..raster::SUBPIXEL_PHASES`）：同一字形在同一 ppem 下每个相位各占
    /// 一个 tile，绘制时按 `整数基准 + placement.left` 落点（见 `raster::split_subpixel_x`）。
    subpixel_x: u8,
    /// 实例坐标 / faux bold 的摘要（见 `GlyphVariation`）：同一字形的不同实例各占一个 tile。
    variation: u64,
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
}

/// `ensure` 的返回值：tile 与落点。
#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphTile {
    pub tile: AtlasTile,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

/// 缓存统计（喂 `TextPassReport`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GlyphAtlasStats {
    pub hits: usize,
    pub misses: usize,
    pub rasters: usize,
    pub rebuilds: usize,
}

pub(crate) struct GlyphAtlas {
    records: ahash::HashMap<GlyphKey, GlyphRecord>,
    /// 反查（P09/P15 的逐字形逐出预留；`clear` 读它，保证不是死字段）。
    ids: ahash::HashMap<GlyphKey, etagere::AllocId>,
    /// 空白/无轮廓字形的**负缓存**（N5：不再每帧重光栅空格）。
    missing: ahash::HashSet<GlyphKey>,
    stats: GlyphAtlasStats,
    /// 整张字形页被清空过（DPI 换代 / 容量逃逸重排）⇒ 旧代际的 tile 全部失效，
    /// 上传侧必须把整页内容重写到 GPU（padding 里可能残留旧字形的覆盖度）。
    cleared: bool,
}

impl GlyphAtlas {
    pub(crate) fn new() -> Self {
        Self {
            records: ahash::HashMap::default(),
            ids: ahash::HashMap::default(),
            missing: ahash::HashSet::default(),
            stats: GlyphAtlasStats::default(),
            cleared: false,
        }
    }

    /// 取字形 tile；未命中则光栅 + 装箱。
    ///
    /// `subpixel_x` 是水平相位桶（`0..raster::SUBPIXEL_PHASES`），参与缓存键：同一字形的不同
    /// 相位必须各占一个 tile，否则亚像素定位会被 tile 复用抹平。
    ///
    /// `Err(AtlasExhausted)` = 清空重建后单块仍放不下（`EngineError::AtlasExhausted`）。
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
            ppem: ppem.to_bits(),
            subpixel_x,
            variation: variation.map_or(0, crate::text::raster::GlyphVariation::hash),
        };
        if self.missing.contains(&key) {
            return Ok(None);
        }
        if let Some(found) = self.records.get(&key).map(|record| {
            (
                record.tile,
                record.left,
                record.top,
                record.width,
                record.height,
                record.data.len(),
            )
        }) && !found.0.is_stale(atlas.generation())
        {
            let (tile, left, top, width, height, data_len) = found;
            debug_assert_eq!(
                data_len as u32,
                width * height,
                "cached R8 coverage must stay complete (N2)"
            );
            self.stats.hits += 1;
            return Ok(Some(GlyphTile {
                tile,
                left,
                top,
                width,
                height,
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
            self.missing.insert(key);
            return Ok(None);
        };
        self.stats.rasters += 1;
        let (width, height) = (image.placement.width, image.placement.height);
        debug_assert_eq!(
            image.data.len() as u32,
            width * height,
            "R8 alpha must be width * height bytes"
        );
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let content = (width, height);
        let mut allocation: Option<Allocation> = atlas.allocate(content);
        if allocation.is_none() {
            // 容量逃逸：先按页档阶梯换代扩容（512 → 1024 → 2048），换到放得下为止；
            // 已是上限则清空同页重试一次（DEVIATIONS F-208）。字形 tile 只由本缓存持有，
            // 换代只需清空缓存，不向应用暴露失效。
            self.stats.rebuilds += 1;
            let mut resized = false;
            while atlas.grow() {
                resized = true;
                allocation = atlas.allocate(content);
                if allocation.is_some() {
                    break;
                }
            }
            if !resized {
                atlas.clear();
            }
            self.clear();
            if allocation.is_none() {
                allocation = atlas.allocate(content);
            }
        }
        let Some(allocation) = allocation else {
            return Err(EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph,
            });
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
        };
        let tile = GlyphTile {
            tile: record.tile,
            left: record.left,
            top: record.top,
            width: record.width,
            height: record.height,
        };
        self.records.insert(key, record);
        self.ids.insert(key, allocation.id);
        debug_assert_eq!(
            self.coverage(key).map_or(0, |data| data.len()) as u32,
            width * height,
            "coverage must be retrievable after insert (P09 upload source)"
        );
        Ok(Some(tile))
    }

    /// 与 `AtlasAllocator::clear()` 同一事务调用。
    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.ids.clear();
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
    /// [`GlyphAtlas::write_rows`]，只碰脏行覆盖到的字形（DEVIATIONS F-211）。
    pub(crate) fn write_page(&self, page: &mut [u8], page_width: u32) {
        for record in self.records.values() {
            write_record_rows(page, page_width, record, 0, record.height);
        }
    }

    /// 只把 `rows` 覆盖到的字形行写进整页缓冲区（增量上传的 CPU 侧）。
    ///
    /// 与 [`GlyphAtlas::write_page`] 的区别：不重写整张页，代价随**脏行数**而不是字形总数
    /// 增长（2000 个字形的窗口打一个字不再整页重写）。
    pub(crate) fn write_rows(
        &self,
        page: &mut [u8],
        page_width: u32,
        rows: &crate::render::DirtyRows,
    ) {
        if rows.is_full() {
            self.write_page(page, page_width);
            return;
        }
        for record in self.records.values() {
            let record_y0 = record.origin.1;
            let record_y1 = record_y0 + record.height;
            for span in rows.spans() {
                let start = span.start.max(record_y0).min(record_y1);
                let end = span.end.max(record_y0).min(record_y1);
                if start < end {
                    write_record_rows(page, page_width, record, start - record_y0, end - record_y0);
                }
            }
        }
    }

    /// 字形覆盖度（P09 的上传源；`None` = 空白/未光栅）。
    pub(crate) fn coverage(&self, key: GlyphKey) -> Option<&[u8]> {
        self.records.get(&key).map(|record| record.data.as_ref())
    }

    /// 取走统计（累加进 `TextPassReport` 后清零）。
    pub(crate) fn take_stats(&mut self) -> GlyphAtlasStats {
        core::mem::take(&mut self.stats)
    }

    /// 仅测试：缓存条目数。
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.records.len()
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
    use super::{GlyphAtlas, GlyphKey};
    use crate::{
        paint::atlas::AtlasKind,
        render::AtlasAllocator,
        text::raster::{GlyphRasterizer, probe_font},
    };

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
            ppem: 16.0f32.to_bits(),
            subpixel_x: 0,
        };
        raster.set_font_epoch(raster.font_epoch() + 1);
        let second_key = GlyphKey {
            variation: 0,
            font: super::FontKey::of(&font, raster.font_epoch()),
            glyph: 36,
            ppem: 16.0f32.to_bits(),
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
}
