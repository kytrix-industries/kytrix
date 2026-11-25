use super::raster::{FontKey, GlyphRasterizer};
use crate::{
    error::EngineError,
    math::DevicePx,
    paint::atlas::{AtlasKind, AtlasTile},
    render::{Allocation, AtlasAllocator, atlas::MIN_ATLAS_PAGE},
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
    color_monochrome: bool,
    last_used_pass: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphTile {
    pub tile: AtlasTile,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub color_monochrome: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GlyphAtlasStats {
    pub hits: usize,
    pub misses: usize,
    pub rasters: usize,
    pub rebuilds: usize,
    pub evictions: usize,
    pub skipped: usize,
    pub skipped_color: usize,
}

pub(crate) const ATLAS_FULL_ESCALATION_FRAMES: u32 = 3;

pub(crate) struct GlyphAtlas {
    records: ahash::HashMap<GlyphKey, GlyphRecord>,
    missing: ahash::HashSet<GlyphKey>,
    color_records: usize,
    color: AtlasAllocator,
    color_cleared: bool,
    stats: GlyphAtlasStats,
    cleared: bool,
    pass: u32,
    evicted: u32,
    eviction_epoch: u32,
    consecutive_soft_fail_passes: u32,
    consecutive_color_soft_fail_passes: u32,
}

impl GlyphAtlas {
    pub(crate) fn new() -> Self {
        Self {
            records: ahash::HashMap::default(),
            missing: ahash::HashSet::default(),
            color_records: 0,
            color: AtlasAllocator::with_page(
                AtlasKind::ColorGlyph,
                (MIN_ATLAS_PAGE, MIN_ATLAS_PAGE),
            ),
            color_cleared: false,
            stats: GlyphAtlasStats::default(),
            cleared: false,
            pass: 0,
            evicted: 0,
            eviction_epoch: 0,
            consecutive_soft_fail_passes: 0,
            consecutive_color_soft_fail_passes: 0,
        }
    }

    pub(crate) fn color_allocator(&self) -> &AtlasAllocator {
        &self.color
    }

    pub(crate) fn color_page(&self) -> (u32, u32) {
        self.color.page()
    }

    pub(crate) fn color_generation(&self) -> u32 {
        self.color.generation()
    }

    pub(crate) fn has_color_records(&self) -> bool {
        self.color_records > 0
    }

    pub(crate) fn color_take_dirty_rects(&mut self) -> crate::render::DirtyRects {
        self.color.take_dirty_rects()
    }

    pub(crate) fn color_take_cleared(&mut self) -> bool {
        core::mem::take(&mut self.color_cleared)
    }

    pub(crate) fn write_color_page(&self, page: &mut [u8], page_width: u32) {
        for record in self
            .records
            .values()
            .filter(|record| record.tile.atlas == AtlasKind::ColorGlyph)
        {
            write_color_record_rows(page, page_width, record, -1, record.height as i32 + 1);
        }
    }

    pub(crate) fn write_color_rows(
        &self,
        page: &mut [u8],
        page_width: u32,
        rects: &crate::render::DirtyRects,
    ) {
        for record in self
            .records
            .values()
            .filter(|record| record.tile.atlas == AtlasKind::ColorGlyph)
        {
            let origin_y = record.origin.1 as i32;
            for rect in rects.rects() {
                let start = rect
                    .y0
                    .value()
                    .max(origin_y - 1)
                    .min(origin_y + record.height as i32 + 1);
                let end = rect
                    .y1
                    .value()
                    .max(origin_y - 1)
                    .min(origin_y + record.height as i32 + 1);
                if start < end {
                    write_color_record_rows(
                        page,
                        page_width,
                        record,
                        start - origin_y,
                        end - origin_y,
                    );
                }
            }
        }
    }

    pub(crate) fn begin_pass(&mut self) {
        self.pass = self.pass.wrapping_add(1);
    }

    pub(crate) fn note_soft_fail(
        &mut self,
        skipped: usize,
        skipped_color: usize,
    ) -> Result<(), EngineError> {
        let escalate = |counter: &mut u32, skipped: usize, kind: AtlasKind| {
            if skipped == 0 {
                *counter = 0;
                return Ok(());
            }
            *counter = counter.saturating_add(1);
            if *counter >= ATLAS_FULL_ESCALATION_FRAMES {
                *counter = 0;
                return Err(EngineError::AtlasExhausted { kind });
            }
            Ok(())
        };
        escalate(
            &mut self.consecutive_soft_fail_passes,
            skipped,
            AtlasKind::Glyph,
        )?;
        escalate(
            &mut self.consecutive_color_soft_fail_passes,
            skipped_color,
            AtlasKind::ColorGlyph,
        )
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
        if let Some(found) = self.records.get_mut(&key) {
            let generation = if found.tile.atlas == AtlasKind::ColorGlyph {
                self.color.generation()
            } else {
                atlas.generation()
            };
            if !found.tile.is_stale(generation) {
                found.last_used_pass = self.pass;
                let record = &*found;
                self.stats.hits += 1;
                return Ok(Some(GlyphTile {
                    tile: record.tile,
                    left: record.left,
                    top: record.top,
                    width: record.width,
                    height: record.height,
                    color_monochrome: record.color_monochrome,
                }));
            }
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
        let texel = if image.color { 4 } else { 1 };
        debug_assert_eq!(
            image.data.len() as u32,
            width * height * texel,
            "glyph raster payload must match {texel} texel bytes"
        );
        if width == 0 || height == 0 {
            self.missing.insert(key);
            return Ok(None);
        }
        self.stats.rasters += 1;
        let kind = if image.color {
            AtlasKind::ColorGlyph
        } else {
            AtlasKind::Glyph
        };
        let content = (width, height);
        let allocation = self.allocate_with_eviction(atlas, kind, content);
        let Some(allocation) = allocation else {
            if image.color {
                self.stats.skipped_color += 1;
            } else {
                self.stats.skipped += 1;
            }
            return Ok(None);
        };
        debug_assert_eq!(allocation.tile.atlas, kind);
        debug_assert_eq!(allocation.content.width().value() as u32, width);
        debug_assert_eq!(allocation.content.height().value() as u32, height);
        debug_assert!(allocation.padded.width() >= allocation.content.width());
        if allocation.tile.atlas == AtlasKind::ColorGlyph {
            self.color.mark_written(allocation.padded);
        }
        let record = GlyphRecord {
            tile: allocation.tile,
            left: image.placement.left,
            top: image.placement.top,
            width,
            height,
            origin: (
                allocation.content.x0.max(DevicePx::ZERO).value() as u32,
                allocation.content.y0.max(DevicePx::ZERO).value() as u32,
            ),
            data: image.data.into_boxed_slice(),
            alloc: allocation.id,
            color_monochrome: image.color_monochrome,
            last_used_pass: self.pass,
        };
        let tile = GlyphTile {
            tile: record.tile,
            left: record.left,
            top: record.top,
            width: record.width,
            height: record.height,
            color_monochrome: record.color_monochrome,
        };
        if record.tile.atlas == AtlasKind::ColorGlyph {
            self.color_records += 1;
        }
        self.records.insert(key, record);
        debug_assert_eq!(
            self.coverage(key).map_or(0, |data| data.len()) as u32,
            width * height * texel,
            "coverage must be retrievable after insert (P09 upload source)"
        );
        Ok(Some(tile))
    }

    fn allocate_with_eviction(
        &mut self,
        atlas: &mut AtlasAllocator,
        kind: AtlasKind,
        content: (u32, u32),
    ) -> Option<Allocation> {
        let color = kind == AtlasKind::ColorGlyph;
        let first = if color {
            self.color.allocate(content)
        } else {
            atlas.allocate(content)
        };
        if let Some(allocation) = first {
            return Some(allocation);
        }
        let mut grown = false;
        while grow_target(&mut self.color, atlas, kind) {
            grown = true;
            let allocation = if color {
                self.color.allocate(content)
            } else {
                atlas.allocate(content)
            };
            if let Some(allocation) = allocation {
                self.stats.rebuilds += 1;
                clear_records(&mut self.records, kind);
                if kind == AtlasKind::ColorGlyph {
                    self.color_records = 0;
                }
                mark_cleared(&mut self.cleared, &mut self.color_cleared, kind);
                return Some(allocation);
            }
        }
        if grown {
            self.stats.rebuilds += 1;
            clear_records(&mut self.records, kind);
            if kind == AtlasKind::ColorGlyph {
                self.color_records = 0;
            }
            mark_cleared(&mut self.cleared, &mut self.color_cleared, kind);
        }
        loop {
            let mut candidates: Vec<(u32, GlyphKey)> = self
                .records
                .iter()
                .filter(|(_, record)| {
                    record.tile.atlas == kind && record.last_used_pass < self.pass
                })
                .map(|(key, record)| (record.last_used_pass, *key))
                .collect();
            if candidates.is_empty() {
                return None;
            }
            candidates.sort_unstable_by_key(|(last_used, _)| *last_used);
            let batch = (candidates.len() / 8).max(1);
            for (_, key) in candidates.into_iter().take(batch) {
                if let Some(record) = self.records.remove(&key) {
                    if record.tile.atlas == AtlasKind::ColorGlyph {
                        self.color_records = self.color_records.saturating_sub(1);
                    }
                    deallocate_target(&mut self.color, atlas, kind, record.alloc);
                    self.evicted = self.evicted.wrapping_add(1);
                    self.eviction_epoch = self.eviction_epoch.wrapping_add(1);
                    self.stats.evictions += 1;
                }
            }
            let allocation = if color {
                self.color.allocate(content)
            } else {
                atlas.allocate(content)
            };
            if let Some(allocation) = allocation {
                return Some(allocation);
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.records.clear();
        self.missing.clear();
        self.color_records = 0;
        self.cleared = true;
        self.color.clear();
        self.color_cleared = true;
    }

    pub(crate) fn take_cleared(&mut self) -> bool {
        core::mem::take(&mut self.cleared)
    }

    pub(crate) fn write_page(&self, page: &mut [u8], page_width: u32) {
        for record in self.records.values() {
            if record.tile.atlas != AtlasKind::Glyph {
                continue;
            }
            write_record_rows(page, page_width, record, 0, record.height, 1);
        }
    }

    pub(crate) fn write_rows(
        &self,
        page: &mut [u8],
        page_width: u32,
        rects: &crate::render::DirtyRects,
    ) {
        write_records_rows(
            page,
            page_width,
            rects,
            self.records
                .values()
                .filter(|record| record.tile.atlas == AtlasKind::Glyph),
            1,
        );
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
    pub(crate) fn color_len(&self) -> usize {
        self.records
            .values()
            .filter(|record| record.tile.atlas == AtlasKind::ColorGlyph)
            .count()
    }

    #[cfg(test)]
    pub(crate) fn missing_len(&self) -> usize {
        self.missing.len()
    }

    #[cfg(test)]
    pub(crate) fn set_color_page_for_test(&mut self, page: u32) {
        self.color.cap_page_for_test(page);
        self.color_cleared = true;
    }

    #[cfg(test)]
    pub(crate) fn set_color_allocator_for_test(&mut self, allocator: AtlasAllocator) {
        self.color = allocator;
        self.color_cleared = true;
    }
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new()
    }
}

fn grow_target(color: &mut AtlasAllocator, atlas: &mut AtlasAllocator, kind: AtlasKind) -> bool {
    if kind == AtlasKind::ColorGlyph {
        color.grow()
    } else {
        atlas.grow()
    }
}

fn deallocate_target(
    color: &mut AtlasAllocator,
    atlas: &mut AtlasAllocator,
    kind: AtlasKind,
    id: etagere::AllocId,
) {
    if kind == AtlasKind::ColorGlyph {
        color.deallocate(id);
    } else {
        atlas.deallocate(id);
    }
}

fn clear_records(records: &mut ahash::HashMap<GlyphKey, GlyphRecord>, kind: AtlasKind) {
    records.retain(|_, record| record.tile.atlas != kind);
}

fn mark_cleared(cleared: &mut bool, color_cleared: &mut bool, kind: AtlasKind) {
    if kind == AtlasKind::ColorGlyph {
        *color_cleared = true;
    } else {
        *cleared = true;
    }
}

fn write_records_rows<'a>(
    page: &mut [u8],
    page_width: u32,
    rects: &crate::render::DirtyRects,
    records: impl Iterator<Item = &'a GlyphRecord>,
    texel: u32,
) {
    for record in records {
        let record_y0 = record.origin.1;
        let record_y1 = record_y0 + record.height;
        for rect in rects.rects() {
            let start = rect
                .y0
                .max(DevicePx::new(record_y0 as i32))
                .value()
                .min(record_y1 as i32);
            let end = rect
                .y1
                .max(DevicePx::new(record_y0 as i32))
                .value()
                .min(record_y1 as i32);
            if start < end {
                write_record_rows(
                    page,
                    page_width,
                    record,
                    (start - record_y0 as i32) as u32,
                    (end - record_y0 as i32) as u32,
                    texel,
                );
            }
        }
    }
}

fn write_record_rows(
    page: &mut [u8],
    page_width: u32,
    record: &GlyphRecord,
    row0: u32,
    row1: u32,
    texel: u32,
) {
    let (x0, y0) = record.origin;
    let row_len = record.width as usize * texel as usize;
    let pitch = page_width as usize * texel as usize;
    for row in row0..row1 {
        let destination = (y0 + row) as usize * pitch + x0 as usize * texel as usize;
        let source = row as usize * row_len;
        if destination + row_len > page.len() || source + row_len > record.data.len() {
            continue;
        }
        page[destination..destination + row_len]
            .copy_from_slice(&record.data[source..source + row_len]);
    }
}

fn write_color_record_rows(
    page: &mut [u8],
    page_width: u32,
    record: &GlyphRecord,
    row0: i32,
    row1: i32,
) {
    let (x0, y0) = record.origin;
    let start = row0.max(-1);
    let end = row1.min(record.height as i32 + 1);
    if start >= end || record.width == 0 || record.height == 0 {
        return;
    }
    let source_pitch = record.width as usize * 4;
    let pitch = page_width as usize * 4;
    let strip_len = (record.width as usize + 2) * 4;
    let mut strip = vec![0u8; strip_len];
    for row in start..end {
        let source_row = row.clamp(0, record.height as i32 - 1) as usize;
        let source_start = source_row * source_pitch;
        let Some(source) = record.data.get(source_start..source_start + source_pitch) else {
            continue;
        };
        strip[0..4].copy_from_slice(&source[0..4]);
        strip[4..4 + source_pitch].copy_from_slice(source);
        strip[4 + source_pitch..strip_len].copy_from_slice(&source[source_pitch - 4..]);
        let destination_row = y0 as i32 + row;
        let destination_column = x0 as i32 - 1;
        if destination_row < 0 || destination_column < 0 {
            continue;
        }
        let destination = destination_row as usize * pitch + destination_column as usize * 4;
        if destination + strip_len > page.len() {
            continue;
        }
        page[destination..destination + strip_len].copy_from_slice(&strip);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ATLAS_FULL_ESCALATION_FRAMES, GlyphAtlas, GlyphAtlasStats, GlyphKey, GlyphRecord, ppem_key,
        write_color_record_rows, write_record_rows,
    };
    use crate::{
        paint::atlas::AtlasKind,
        render::{AtlasAllocator, DirtyRects},
        text::raster::{FontKey, GlyphRasterizer, probe_emoji_font, probe_font},
    };

    fn fabricated_color_record(allocator: &mut AtlasAllocator, data: &[u8]) -> GlyphRecord {
        let allocation = allocator.allocate((2, 2)).expect("2x2 tile fits");
        GlyphRecord {
            tile: allocation.tile,
            left: 0,
            top: 0,
            width: 2,
            height: 2,
            origin: (
                allocation.content.x0.value() as u32,
                allocation.content.y0.value() as u32,
            ),
            data: data.to_vec().into_boxed_slice(),
            alloc: allocation.id,
            color_monochrome: false,
            last_used_pass: 0,
        }
    }

    fn page_pixel(page: &[u8], page_width: u32, x: u32, y: u32) -> [u8; 4] {
        let offset = (y as usize * page_width as usize + x as usize) * 4;
        page[offset..offset + 4].try_into().expect("4 bytes")
    }

    fn sample_bilinear(page: &[u8], page_width: u32, x: f64, y: f64) -> [f64; 4] {
        let x = x - 0.5;
        let y = y - 0.5;
        let x0 = x.floor() as i64;
        let y0 = y.floor() as i64;
        let fx = x - x0 as f64;
        let fy = y - y0 as f64;
        let fetch = |ix: i64, iy: i64| -> [f64; 4] {
            let ix = ix.clamp(0, i64::from(page_width) - 1) as u32;
            let iy = iy.clamp(0, i64::from(page_width) - 1) as u32;
            page_pixel(page, page_width, ix, iy).map(|channel| f64::from(channel) / 255.0)
        };
        let top = [fetch(x0, y0), fetch(x0 + 1, y0)];
        let bottom = [fetch(x0, y0 + 1), fetch(x0 + 1, y0 + 1)];
        core::array::from_fn(|channel| {
            let top = top[0][channel] * (1.0 - fx) + top[1][channel] * fx;
            let bottom = bottom[0][channel] * (1.0 - fx) + bottom[1][channel] * fx;
            top * (1.0 - fy) + bottom * fy
        })
    }

    fn full_page(page_width: u32) -> Vec<u8> {
        vec![0u8; (page_width * page_width * 4) as usize]
    }

    #[test]
    fn color_glyph_page_writes_replicate_the_tile_edges_into_the_padding() {
        let mut allocator = AtlasAllocator::with_page(AtlasKind::ColorGlyph, (32, 32));
        let data = [
            10u8, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        let record = fabricated_color_record(&mut allocator, &data);
        let (x0, y0) = record.origin;
        let width = 32;
        let mut page = full_page(width);
        write_color_record_rows(&mut page, width, &record, -1, 3);

        assert_eq!(page_pixel(&page, width, x0, y0), [10, 20, 30, 255]);
        assert_eq!(
            page_pixel(&page, width, x0 + 1, y0 + 1),
            [100, 110, 120, 255]
        );
        assert_eq!(page_pixel(&page, width, x0 - 1, y0), [10, 20, 30, 255]);
        assert_eq!(page_pixel(&page, width, x0 + 2, y0), [40, 50, 60, 255]);
        assert_eq!(page_pixel(&page, width, x0 - 1, y0 + 1), [70, 80, 90, 255]);
        assert_eq!(
            page_pixel(&page, width, x0 + 2, y0 + 1),
            [100, 110, 120, 255]
        );
        assert_eq!(page_pixel(&page, width, x0, y0 - 1), [10, 20, 30, 255]);
        assert_eq!(page_pixel(&page, width, x0 + 1, y0 - 1), [40, 50, 60, 255]);
        assert_eq!(page_pixel(&page, width, x0, y0 + 2), [70, 80, 90, 255]);
        assert_eq!(
            page_pixel(&page, width, x0 + 1, y0 + 2),
            [100, 110, 120, 255]
        );
        assert_eq!(page_pixel(&page, width, x0 - 1, y0 - 1), [10, 20, 30, 255]);
        assert_eq!(
            page_pixel(&page, width, x0 + 2, y0 + 2),
            [100, 110, 120, 255]
        );
    }

    #[test]
    fn color_glyph_partial_row_writes_also_cover_the_padding_rows() {
        let mut allocator = AtlasAllocator::with_page(AtlasKind::ColorGlyph, (32, 32));
        let data = [
            10u8, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        let record = fabricated_color_record(&mut allocator, &data);
        let (x0, y0) = record.origin;
        let width = 32;
        let mut glyphs = GlyphAtlas::new();
        glyphs.records.insert(
            GlyphKey {
                font: FontKey::of(
                    &parley::FontData::new(parley::fontique::Blob::from(Vec::new()), 0),
                    1,
                ),
                glyph: 0,
                ppem_q: 0,
                subpixel_x: 0,
                variation: 0,
            },
            GlyphRecord {
                tile: record.tile,
                left: record.left,
                top: record.top,
                width: record.width,
                height: record.height,
                origin: record.origin,
                data: record.data.clone(),
                alloc: record.alloc,
                color_monochrome: record.color_monochrome,
                last_used_pass: record.last_used_pass,
            },
        );
        let mut page = full_page(width);
        glyphs.write_color_rows(&mut page, width, &DirtyRects::full());

        assert_eq!(page_pixel(&page, width, x0 - 1, y0 - 1), [10, 20, 30, 255]);
        assert_eq!(page_pixel(&page, width, x0 + 2, y0 - 1), [40, 50, 60, 255]);
        assert_eq!(page_pixel(&page, width, x0 - 1, y0 + 2), [70, 80, 90, 255]);
        assert_eq!(
            page_pixel(&page, width, x0 + 2, y0 + 2),
            [100, 110, 120, 255]
        );
    }

    #[test]
    fn color_glyph_padding_stops_linear_filtering_from_darkening_the_edges() {
        let mut allocator = AtlasAllocator::with_page(AtlasKind::ColorGlyph, (32, 32));
        let opaque = [200u8, 80, 40, 255];
        let data: Vec<u8> = opaque.iter().copied().cycle().take(16).collect();
        let record = fabricated_color_record(&mut allocator, &data);
        let (x0, y0) = record.origin;
        let width = 32;
        let mut padded = full_page(width);
        write_color_record_rows(&mut padded, width, &record, -1, 3);
        let mut unpadded = full_page(width);
        write_record_rows(&mut unpadded, width, &record, 0, 2, 4);

        let sample = |page: &[u8]| sample_bilinear(page, width, f64::from(x0), f64::from(y0) + 0.5);
        let premultiply = |sample: [f64; 4]| {
            [
                sample[0] * sample[3],
                sample[1] * sample[3],
                sample[2] * sample[3],
            ]
        };
        let padded_out = premultiply(sample(&padded));
        let unpadded_out = premultiply(sample(&unpadded));
        assert!(
            padded_out[0] > 0.7,
            "padding 复制边缘色后，tile 边界（u0/v 边）的 50/50 LINEAR 采样必须保留整块颜色，实际 premult r={}",
            padded_out[0]
        );
        assert!(
            unpadded_out[0] < 0.35,
            "padding 全 0 时边界采样与透明黑混合，premult r 只剩约一半，实际 {}",
            unpadded_out[0]
        );
        assert!(
            padded_out[0] > unpadded_out[0] * 2.0,
            "padding 复制必须消除边缘偏暗：padded={:?} unpadded={:?}",
            padded_out,
            unpadded_out
        );
    }

    #[test]
    fn color_glyph_allocation_marks_the_tile_padding_dirty() {
        let Some(emoji) = probe_emoji_font() else {
            eprintln!("skip: no color emoji font available on this device");
            return;
        };
        let emoji_id = u32::from(
            swash::FontRef::from_index(emoji.data.data(), emoji.index as usize)
                .expect("emoji font")
                .charmap()
                .map('\u{1F30D}'),
        );
        assert_ne!(emoji_id, 0);
        let mut raster = GlyphRasterizer::new();
        let mut glyphs = GlyphAtlas::new();
        let mut atlas = tiny_glyph_atlas();
        let tile = glyphs
            .ensure(&mut raster, &mut atlas, &emoji, emoji_id, 32.0, 0, None)
            .expect("ok")
            .expect("emoji coverage");
        assert_eq!(tile.tile.atlas, AtlasKind::ColorGlyph);
        let rects = glyphs.color_take_dirty_rects();
        assert!(
            rects.rects().iter().any(|rect| {
                rect.width().value() as u32 >= tile.width + 2
                    && rect.height().value() as u32 >= tile.height + 2
            }),
            "彩色 tile 分配必须把 1px padding 一起标脏（上传必须覆盖 padding）：tile={}x{} rects={:?}",
            tile.width,
            tile.height,
            rects.rects()
        );
    }

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
    fn color_emoji_go_to_the_color_atlas_and_mono_glyphs_stay_in_r8() {
        let Some(emoji) = probe_emoji_font() else {
            eprintln!("skip: no color emoji font available on this device");
            return;
        };
        let Some(mono) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let emoji_id = u32::from(
            swash::FontRef::from_index(emoji.data.data(), emoji.index as usize)
                .expect("emoji font")
                .charmap()
                .map('\u{1F30D}'),
        );
        assert_ne!(emoji_id, 0);
        let mono_id = u32::from(
            swash::FontRef::from_index(mono.data.data(), mono.index as usize)
                .expect("system font")
                .charmap()
                .map('A'),
        );
        let mut raster = GlyphRasterizer::new();
        let mut glyphs = GlyphAtlas::new();
        let mut atlas = tiny_glyph_atlas();

        let color_tile = glyphs
            .ensure(&mut raster, &mut atlas, &emoji, emoji_id, 32.0, 0, None)
            .expect("ok")
            .expect("emoji coverage");
        assert_eq!(
            color_tile.tile.atlas,
            AtlasKind::ColorGlyph,
            "彩色字形必须进 RGBA 图集"
        );
        assert_eq!(glyphs.color_len(), 1);
        assert_eq!(glyphs.len(), 1);
        let key = GlyphKey {
            font: FontKey::of(&emoji, raster.font_epoch()),
            glyph: emoji_id,
            ppem_q: ppem_key(32.0),
            subpixel_x: 0,
            variation: 0,
        };
        let coverage = glyphs.coverage(key).expect("彩色覆盖度");
        assert_eq!(
            coverage.len() as u32,
            color_tile.width * color_tile.height * 4,
            "彩色 tile 载荷是 RGBA8（4 B/texel）"
        );
        assert!(
            coverage
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] != 0),
            "彩色覆盖度必须有非透明像素"
        );
        assert!(
            coverage.as_chunks::<4>().0.iter().any(|pixel| {
                let max = pixel[..3].iter().copied().max().unwrap_or(0);
                let min = pixel[..3].iter().copied().min().unwrap_or(0);
                pixel[3] > 200 && max - min > 60
            }),
            "彩色覆盖度必须保留颜色（只有 alpha 就说明仍走单色路径）"
        );

        let mono_tile = glyphs
            .ensure(&mut raster, &mut atlas, &mono, mono_id, 32.0, 0, None)
            .expect("ok")
            .expect("mono coverage");
        assert_eq!(mono_tile.tile.atlas, AtlasKind::Glyph);
        assert_eq!(glyphs.color_len(), 1, "单色字形不得进彩色图集");
        assert_eq!(glyphs.len(), 2, "同一 GlyphKey 命名空间共享记录表");

        let hit = glyphs
            .ensure(&mut raster, &mut atlas, &emoji, emoji_id, 32.0, 0, None)
            .expect("ok")
            .expect("cached emoji");
        assert_eq!(hit.tile, color_tile.tile, "同键必须命中同一个彩色 tile");
    }

    #[test]
    fn color_atlas_lru_eviction_reclaims_color_records_and_deallocates_their_tiles() {
        let Some(emoji) = probe_emoji_font() else {
            eprintln!("skip: no color emoji font available on this device");
            return;
        };
        let Some(mono) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let emoji_ref = swash::FontRef::from_index(emoji.data.data(), emoji.index as usize)
            .expect("emoji font");
        let emojis: Vec<u32> = [
            '\u{1F30D}',
            '\u{1F468}',
            '\u{1F469}',
            '\u{1F467}',
            '\u{1F412}',
        ]
        .into_iter()
        .map(|ch| u32::from(emoji_ref.charmap().map(ch)))
        .filter(|id| *id != 0)
        .collect();
        if emojis.len() < 2 {
            eprintln!("skip: emoji font does not map enough emoji glyphs");
            return;
        }
        let mono_id = u32::from(
            swash::FontRef::from_index(mono.data.data(), mono.index as usize)
                .expect("system font")
                .charmap()
                .map('A'),
        );
        let mut probe = GlyphRasterizer::new();
        let variations = crate::text::raster::GlyphVariation::default();
        let first = probe
            .rasterize(&emoji, emojis[0], 16.0, 0, &variations)
            .expect("first emoji must rasterize");
        let second = probe
            .rasterize(&emoji, emojis[1], 16.0, 0, &variations)
            .expect("second emoji must rasterize");
        let page = [
            first.placement.width + 2,
            first.placement.height + 2,
            second.placement.width + 2,
            second.placement.height + 2,
        ]
        .into_iter()
        .max()
        .expect("tile size");
        assert!(
            page < u32::from(u16::MAX),
            "注入的彩色页必须可表达为 u32 页尺寸"
        );

        let mut raster = GlyphRasterizer::new();
        let mut glyphs = GlyphAtlas::new();
        let mut atlas = tiny_glyph_atlas();
        let mono_tile = glyphs
            .ensure(&mut raster, &mut atlas, &mono, mono_id, 32.0, 0, None)
            .expect("ok")
            .expect("mono coverage");
        glyphs.begin_pass();
        glyphs.set_color_page_for_test(page);
        assert!(glyphs.color_take_cleared());
        let first_tile = glyphs
            .ensure(&mut raster, &mut atlas, &emoji, emojis[0], 16.0, 0, None)
            .expect("ok")
            .expect("first emoji must fit the capped color page");
        assert_eq!(first_tile.tile.atlas, AtlasKind::ColorGlyph);
        assert_eq!(glyphs.color_len(), 1);
        assert!(glyphs.has_color_records());
        let _ = glyphs.take_stats();

        glyphs.begin_pass();
        let second_tile = glyphs
            .ensure(&mut raster, &mut atlas, &emoji, emojis[1], 16.0, 0, None)
            .expect("ok")
            .expect("the LRU color tile must be evicted to make room");
        assert_eq!(second_tile.tile.atlas, AtlasKind::ColorGlyph);
        assert_ne!(
            second_tile.tile, first_tile.tile,
            "第二块彩色 tile 必须复用被驱逐的槽位（换代身份不同）"
        );
        let stats = glyphs.take_stats();
        assert_eq!(
            stats.evictions, 1,
            "彩色页只放得下一个 tile：第二块必须真正驱逐第一块（stats: {stats:?}）"
        );
        assert_eq!(
            stats.skipped_color, 0,
            "驱逐后必须成功分配，不能退化成软失败（stats: {stats:?}）"
        );
        assert_eq!(glyphs.color_len(), 1);
        assert!(glyphs.has_color_records());
        let mono_hit = glyphs
            .ensure(&mut raster, &mut atlas, &mono, mono_id, 32.0, 0, None)
            .expect("ok")
            .expect("mono tile must survive color eviction");
        assert_eq!(mono_hit.tile, mono_tile.tile, "彩色淘汰不得波及 R8 图集");
        assert_eq!(mono_hit.tile.atlas, AtlasKind::Glyph);
        let _ = glyphs.take_stats();

        glyphs.begin_pass();
        let oversized = glyphs
            .ensure(&mut raster, &mut atlas, &emoji, emojis[0], 64.0, 0, None)
            .expect("ok");
        assert!(
            oversized.is_none(),
            "64 ppem 的彩色 tile 大于注入页：必须软失败"
        );
        let stats = glyphs.take_stats();
        assert_eq!(
            stats.skipped_color, 1,
            "超大彩色 tile 必须计一次彩色软失败（stats: {stats:?}）"
        );
        assert_eq!(stats.evictions, 1, "驱逐最后一条彩色记录后不得再驱逐");
        assert!(
            !glyphs.has_color_records(),
            "驱逐最后一条彩色记录必须把 color_records 递减到 0（否则彩色页会被永久认为有记录）"
        );
        assert_eq!(glyphs.color_len(), 0);
    }

    #[test]
    fn color_atlas_growth_rebuilds_only_the_color_records() {
        let Some(emoji) = probe_emoji_font() else {
            eprintln!("skip: no color emoji font available on this device");
            return;
        };
        let Some(mono) = probe_font() else {
            eprintln!("skip: no system font available");
            return;
        };
        let emoji_id = u32::from(
            swash::FontRef::from_index(emoji.data.data(), emoji.index as usize)
                .expect("emoji font")
                .charmap()
                .map('\u{1F30D}'),
        );
        let mono_id = u32::from(
            swash::FontRef::from_index(mono.data.data(), mono.index as usize)
                .expect("system font")
                .charmap()
                .map('A'),
        );
        let mut raster = GlyphRasterizer::new();
        let mut glyphs = GlyphAtlas::new();
        let mut atlas = tiny_glyph_atlas();
        let mono_tile = glyphs
            .ensure(&mut raster, &mut atlas, &mono, mono_id, 32.0, 0, None)
            .expect("ok")
            .expect("mono coverage");
        glyphs
            .set_color_allocator_for_test(AtlasAllocator::with_page(AtlasKind::ColorGlyph, (1, 1)));
        assert!(glyphs.color_take_cleared());
        let old_generation = glyphs.color_generation();
        let color_tile = glyphs
            .ensure(&mut raster, &mut atlas, &emoji, emoji_id, 32.0, 0, None)
            .expect("growth must rescue the color glyph")
            .expect("emoji coverage");
        assert_eq!(glyphs.color_page(), (512, 512), "彩色页按阶梯长到 512");
        assert_ne!(glyphs.color_generation(), old_generation, "扩容必须换代");
        assert!(glyphs.color_take_cleared(), "扩容后必须整页重传");
        let stats = glyphs.take_stats();
        assert_eq!(stats.rebuilds, 1);
        assert_eq!(color_tile.tile.generation, glyphs.color_generation());
        let mono_again = glyphs
            .ensure(&mut raster, &mut atlas, &mono, mono_id, 32.0, 0, None)
            .expect("ok")
            .expect("mono tile survives color rebuild");
        assert_eq!(
            mono_again.tile, mono_tile.tile,
            "彩色图集扩容不得清掉 R8 图集的字形"
        );
    }

    #[test]
    fn color_glyph_atlas_memory_budget_is_registered() {
        let steps = crate::render::atlas::ATLAS_PAGE_STEPS;
        assert_eq!(steps, [512, 1024, 2048], "彩色页复用单色页的阶梯");
        let bytes: Vec<u64> = steps
            .iter()
            .map(|page| u64::from(*page) * u64::from(*page) * 4)
            .collect();
        assert_eq!(
            bytes,
            vec![1 << 20, 4 << 20, 16 << 20],
            "RGBA8 页预算：512²=1 MiB、1024²=4 MiB、2048²=16 MiB"
        );
        let glyphs = GlyphAtlas::new();
        let (width, height) = glyphs.color_page();
        assert_eq!((width, height), (512, 512), "彩色图集默认开一页 512²");
        assert_eq!(
            u64::from(width) * u64::from(height) * 4,
            1 << 20,
            "默认常驻 1 MiB（未放任何彩色字形时为 0 条记录）"
        );
        assert_eq!(glyphs.color_len(), 0);
        let mono_page = tiny_glyph_atlas().page();
        assert_eq!(
            u64::from(mono_page.0) * u64::from(mono_page.1),
            (256 * 256) as u64,
            "对照：单色 R8 页每 texel 1 B（测试页 256² = 64 KiB）"
        );
    }

    #[test]
    fn consecutive_color_soft_fail_passes_escalate_to_color_glyph_exhausted() {
        let mut glyphs = GlyphAtlas::new();
        for _ in 1..ATLAS_FULL_ESCALATION_FRAMES {
            assert!(glyphs.note_soft_fail(0, 1).is_ok());
        }
        let error = glyphs
            .note_soft_fail(0, 1)
            .expect_err("连续彩色软失败必须升级为 ColorGlyph 图集耗尽");
        assert!(matches!(
            error,
            crate::error::EngineError::AtlasExhausted {
                kind: AtlasKind::ColorGlyph
            }
        ));
        assert!(
            glyphs.note_soft_fail(1, 0).is_ok(),
            "彩色计数清零后，单色软失败必须独立计数"
        );
    }

    #[test]
    fn consecutive_soft_fail_passes_escalate_to_atlas_exhausted() {
        let mut glyphs = GlyphAtlas::new();
        for pass in 1..ATLAS_FULL_ESCALATION_FRAMES {
            assert!(
                glyphs.note_soft_fail(2, 0).is_ok(),
                "第 {pass} 个连续软失败 pass 还没到阈值"
            );
        }
        assert!(glyphs.note_soft_fail(0, 0).is_ok());
        for _ in 1..ATLAS_FULL_ESCALATION_FRAMES {
            assert!(
                glyphs.note_soft_fail(1, 0).is_ok(),
                "一次干净 pass 之后必须重新计数"
            );
        }
        let error = glyphs
            .note_soft_fail(1, 0)
            .expect_err("连续软失败达到阈值必须升级为 EngineError::AtlasExhausted");
        assert!(matches!(
            error,
            crate::error::EngineError::AtlasExhausted {
                kind: AtlasKind::Glyph
            }
        ));
        assert!(
            glyphs.note_soft_fail(1, 0).is_ok(),
            "升级后必须清零，避免每帧都报错"
        );
    }
}
