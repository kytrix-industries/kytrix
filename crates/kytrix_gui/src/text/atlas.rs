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

/// 字形缓存键：字体身份 + 字形 id + 物理 ppem。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GlyphKey {
    font: FontKey,
    glyph: u32,
    ppem: u32,
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
    /// 有新的覆盖度写入 ⇒ 下一帧要把整页传上去（P09 全页上传；增量属 P15）。
    page_dirty: bool,
}

impl GlyphAtlas {
    pub(crate) fn new() -> Self {
        Self {
            records: ahash::HashMap::default(),
            ids: ahash::HashMap::default(),
            missing: ahash::HashSet::default(),
            stats: GlyphAtlasStats::default(),
            page_dirty: false,
        }
    }

    /// 取字形 tile；未命中则光栅 + 装箱。
    ///
    /// `Err(AtlasExhausted)` = 清空重建后单块仍放不下（`EngineError::AtlasExhausted`）。
    pub(crate) fn ensure(
        &mut self,
        raster: &mut GlyphRasterizer,
        atlas: &mut AtlasAllocator,
        font: &parley::FontData,
        glyph: u32,
        ppem: f32,
    ) -> Result<Option<GlyphTile>, EngineError> {
        let key = GlyphKey {
            font: FontKey::of(font, raster.font_epoch()),
            glyph,
            ppem: ppem.to_bits(),
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

        let Some(image) = raster.rasterize(font, glyph, ppem) else {
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
            // 容量逃逸：清空 + 换代 + 重试一次（P02 §10.6）。
            atlas.clear();
            self.clear();
            self.stats.rebuilds += 1;
            allocation = atlas.allocate(content);
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
        // 新覆盖度：下一帧要整页上传（P09）。
        self.page_dirty = true;
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
        self.page_dirty = true;
    }

    /// 取走「页面内容变了」的信号（`App::frame` 据此上传整页）。
    pub(crate) fn take_page_dirty(&mut self) -> bool {
        core::mem::take(&mut self.page_dirty)
    }

    /// 把所有 tile 的覆盖度写进整页缓冲区（`R8`、行主序、页宽 `page_width`）。
    ///
    /// P09 的上传是**整页**（每批新字形 4 MiB）；脏行增量属 P15（DEVIATIONS V-64）。
    pub(crate) fn write_page(&self, page: &mut [u8], page_width: u32) {
        for record in self.records.values() {
            let (x0, y0) = record.origin;
            let row_len = record.width as usize;
            for row in 0..record.height {
                let destination = (y0 + row) as usize * page_width as usize + x0 as usize;
                let source = row as usize * row_len;
                if destination + row_len > page.len() {
                    continue;
                }
                page[destination..destination + row_len]
                    .copy_from_slice(&record.data[source..source + row_len]);
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

#[cfg(test)]
mod tests {
    use super::{GlyphAtlas, GlyphKey};
    use crate::{
        paint::atlas::AtlasKind,
        render::AtlasAllocator,
        text::raster::{GlyphRasterizer, probe_font},
    };

    fn tiny_glyph_atlas() -> AtlasAllocator {
        AtlasAllocator::for_test(AtlasKind::Glyph, (256, 256))
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

        let first = glyphs.ensure(&mut raster, &mut atlas, &font, 36, 16.0);
        let second = glyphs.ensure(&mut raster, &mut atlas, &font, 36, 16.0);
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
            .ensure(&mut raster, &mut atlas, &font, 36, 13.0)
            .expect("ok");
        let large = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 26.0)
            .expect("ok");
        let (small, large) = (small.expect("13ppem"), large.expect("26ppem"));

        assert_ne!(
            small.tile.uv(),
            large.tile.uv(),
            "different ppem ⇒ different tile"
        );
        assert!(large.width > small.width);
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
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0)
            .expect("ok")
            .expect("coverage");
        atlas.clear();
        glyphs.clear();
        let second = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0)
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
            font: super::FontKey::of(&font, raster.font_epoch()),
            glyph: 36,
            ppem: 16.0f32.to_bits(),
        };
        raster.set_font_epoch(raster.font_epoch() + 1);
        let second_key = GlyphKey {
            font: super::FontKey::of(&font, raster.font_epoch()),
            glyph: 36,
            ppem: 16.0f32.to_bits(),
        };

        assert_ne!(first_key, second_key, "epoch bump must change the key");
    }

    #[test]
    fn atlas_exhaustion_reports_the_error_after_a_retry() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        // 1×1 的页放不下任何带 padding 的字形 ⇒ clear+retry 后仍 None ⇒ AtlasExhausted。
        let mut atlas = AtlasAllocator::for_test(AtlasKind::Glyph, (1, 1));
        let mut glyphs = GlyphAtlas::new();

        let error = glyphs
            .ensure(&mut raster, &mut atlas, &font, 36, 16.0)
            .expect_err("1x1 page cannot hold a glyph");
        assert!(matches!(
            error,
            crate::error::EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph
            }
        ));
        assert_eq!(glyphs.take_stats().rebuilds, 1, "one clear+retry attempt");
    }
}
