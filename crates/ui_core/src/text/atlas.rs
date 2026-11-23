use super::raster::{FontKey, GlyphRasterizer};
use crate::{
    error::EngineError,
    paint::atlas::{AtlasKind, AtlasTile},
    render::{Allocation, AtlasAllocator},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GlyphKey {
    font: FontKey,
    glyph: u32,
    ppem_q: u32,
    subpixel_x: u8,
    variation: u64,
}

pub(crate) const PPEM_KEY_QUANTIZATION: f32 = 64.0;

fn ppem_key(ppem: f32) -> u32 {
    (ppem * PPEM_KEY_QUANTIZATION).round() as u32
}

struct GlyphRecord {
    tile: AtlasTile,
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    origin: (u32, u32),
    data: Box<[u8]>,
    alloc: etagere::AllocId,
    color: bool,
    last_used_pass: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphTile {
    pub tile: AtlasTile,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub color: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GlyphAtlasStats {
    pub hits: usize,
    pub misses: usize,
    pub rasters: usize,
    pub rebuilds: usize,
    pub evictions: usize,
    pub skipped: usize,
}

pub(crate) const ATLAS_FULL_ESCALATION_FRAMES: u32 = 3;

pub(crate) struct GlyphAtlas {
    records: ahash::HashMap<GlyphKey, GlyphRecord>,
    missing: ahash::HashSet<GlyphKey>,
    stats: GlyphAtlasStats,
    cleared: bool,
    pass: u32,
    evicted: u32,
    eviction_epoch: u32,
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

    pub(crate) fn begin_pass(&mut self) {
        self.pass = self.pass.wrapping_add(1);
    }

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

    pub(crate) fn take_evicted(&mut self) -> u32 {
        core::mem::take(&mut self.evicted)
    }

    pub(crate) fn eviction_epoch(&self) -> u32 {
        self.eviction_epoch
    }

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
            self.missing.insert(key);
            return Ok(None);
        }
        self.stats.rasters += 1;
        let content = (width, height);
        let allocation = self.allocate_with_eviction(atlas, content);
        let Some(allocation) = allocation else {
            self.stats.skipped += 1;
            return Ok(None);
        };
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

    fn allocate_with_eviction(
        &mut self,
        atlas: &mut AtlasAllocator,
        content: (u32, u32),
    ) -> Option<Allocation> {
        if let Some(allocation) = atlas.allocate(content) {
            return Some(allocation);
        }
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

    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.missing.clear();
        self.cleared = true;
    }

    pub(crate) fn take_cleared(&mut self) -> bool {
        core::mem::take(&mut self.cleared)
    }

    pub(crate) fn write_page(&self, page: &mut [u8], page_width: u32) {
        for record in self.records.values() {
            write_record_rows(page, page_width, record, 0, record.height);
        }
    }

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

    pub(crate) fn coverage(&self, key: GlyphKey) -> Option<&[u8]> {
        self.records.get(&key).map(|record| record.data.as_ref())
    }

    pub(crate) fn take_stats(&mut self) -> GlyphAtlasStats {
        core::mem::take(&mut self.stats)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

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

    #[test]
    fn missing_cache_is_keyed_by_quantized_ppem() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();
        let missing_glyph = u32::from(u16::MAX);

        assert!(
            glyphs
                .ensure(&mut raster, &mut atlas, &font, missing_glyph, 17.5, 0, None)
                .expect("no error")
                .is_none()
        );
        let stats = glyphs.take_stats();
        assert_eq!((stats.misses, stats.rasters), (1, 0));

        assert!(
            glyphs
                .ensure(&mut raster, &mut atlas, &font, missing_glyph, 17.5, 0, None)
                .expect("no error")
                .is_none()
        );
        assert_eq!(glyphs.take_stats(), super::GlyphAtlasStats::default());

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

    #[test]
    fn zero_size_glyphs_are_negatively_cached() {
        let Some(font) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let mut raster = GlyphRasterizer::new();
        let mut atlas = tiny_glyph_atlas();
        let mut glyphs = GlyphAtlas::new();
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

        glyphs.begin_pass();
        for glyph_id in 36..48u32 {
            let _ = glyphs
                .ensure(&mut raster, &mut atlas, &font, glyph_id, 16.0, 0, None)
                .expect("容量压力绝不该变成错误");
        }
        assert_eq!(glyphs.eviction_epoch(), 0, "只填充不淘汰 ⇒ 纪元不动");

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

    #[test]
    fn consecutive_soft_fail_passes_escalate_to_atlas_exhausted() {
        let mut glyphs = GlyphAtlas::new();
        for pass in 1..ATLAS_FULL_ESCALATION_FRAMES {
            assert!(
                glyphs.note_soft_fail(2).is_ok(),
                "第 {pass} 个连续软失败 pass 还没到阈值"
            );
        }
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
        assert!(
            glyphs.note_soft_fail(1).is_ok(),
            "升级后必须清零，避免每帧都报错"
        );
    }
}
