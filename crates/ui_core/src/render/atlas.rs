use etagere::{AllocId, AtlasAllocator as EtagereAllocator, size2};

use crate::{
    math::{DeviceRect, dp},
    paint::atlas::{AtlasKind, AtlasTile, MAX_IMAGE_PAGES},
};

pub(crate) const MAX_ATLAS_PAGE: u32 = 2048;

pub(crate) const MIN_ATLAS_PAGE: u32 = 512;

pub(crate) const ATLAS_PAGE_STEPS: [u32; 3] = [MIN_ATLAS_PAGE, MIN_ATLAS_PAGE * 2, MAX_ATLAS_PAGE];

pub(crate) const MAX_RESIDENT_STAGING_BYTES: u64 = 1 << 20;

fn next_page(page: u32, max_page: u32) -> Option<u32> {
    ATLAS_PAGE_STEPS
        .iter()
        .copied()
        .filter(|candidate| *candidate <= max_page)
        .find(|candidate| *candidate > page)
}

pub(crate) fn texel_bytes(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Glyph => 1,
        AtlasKind::Image => 4,
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AtlasArraySpec {
    pub page: u32,
    pub layers: u32,
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AtlasPageRequest {
    pub kind: AtlasKind,
    pub spec: AtlasArraySpec,
}

#[cfg(any(windows, target_os = "linux"))]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RectChunk {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub offset: usize,
}

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

#[derive(Clone, Copy, Debug)]
struct Slot {
    id: AllocId,
    epoch: u32,
}

pub(crate) struct AtlasAllocator {
    max_page: u32,
    kind: AtlasKind,
    page: (u32, u32),
    page_index: u16,
    allocator: EtagereAllocator,
    tiles: Vec<Option<Slot>>,
    free: Vec<usize>,
    next_epoch: u32,
    generation: u32,
    dirty: DirtyRects,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Allocation {
    pub id: AllocId,
    pub tile: AtlasTile,
    pub content: DeviceRect,
    pub padded: DeviceRect,
}

impl AtlasAllocator {
    pub(crate) fn with_page(kind: AtlasKind, page: (u32, u32)) -> Self {
        Self {
            max_page: MAX_ATLAS_PAGE,
            kind,
            page,
            page_index: 0,
            allocator: EtagereAllocator::new(size2(page.0 as i32, page.1 as i32)),
            tiles: Vec::with_capacity(64),
            free: Vec::new(),
            next_epoch: 1,
            generation: 1,
            dirty: DirtyRects::default(),
        }
    }

    pub(crate) fn set_page_index(&mut self, page_index: u16) {
        self.page_index = page_index;
    }

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

    pub(crate) fn allocate(&mut self, content: (u32, u32)) -> Option<Allocation> {
        let (width, height) = content;
        if width == 0 || height == 0 {
            return None;
        }
        let padded = (width.checked_add(2)?, height.checked_add(2)?);
        if padded.0 > u32::from(u16::MAX) || padded.1 > u32::from(u16::MAX) {
            return None;
        }
        if padded.0 > self.page().0 || padded.1 > self.page().1 {
            return None;
        }
        let allocation = self
            .allocator
            .allocate(size2(padded.0 as i32, padded.1 as i32))?;
        let rectangle = allocation.rectangle;
        let origin_x = rectangle.min.x + 1;
        let origin_y = rectangle.min.y + 1;
        let slot = match self.free.pop() {
            Some(index) => index,
            None => {
                self.tiles.push(None);
                self.tiles.len() - 1
            }
        };
        let slot_epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.wrapping_add(1);
        self.tiles[slot] = Some(Slot {
            id: allocation.id,
            epoch: slot_epoch,
        });
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
            slot: slot as u32,
            slot_epoch,
        };
        let content_rect = DeviceRect::new(
            dp(origin_x),
            dp(origin_y),
            dp(origin_x + width as i32),
            dp(origin_y + height as i32),
        );
        let padded_rect = DeviceRect::new(
            dp(rectangle.min.x),
            dp(rectangle.min.y),
            dp(rectangle.max.x),
            dp(rectangle.max.y),
        );
        debug_assert_eq!(tile.atlas, self.kind(), "tile must carry its atlas kind");
        self.mark_written(content_rect);
        Some(Allocation {
            id: allocation.id,
            tile,
            content: content_rect,
            padded: padded_rect,
        })
    }

    pub(crate) fn deallocate(&mut self, id: AllocId) {
        let Some(index) = self
            .tiles
            .iter()
            .position(|slot| matches!(slot, Some(existing) if existing.id == id))
        else {
            return;
        };
        self.tiles[index] = None;
        self.free.push(index);
        self.allocator.deallocate(id);
    }

    pub(crate) fn is_live(&self, id: AllocId) -> bool {
        self.tiles
            .iter()
            .any(|slot| matches!(slot, Some(existing) if existing.id == id))
    }

    pub(crate) fn is_alive(&self, tile: &AtlasTile) -> bool {
        tile.atlas == self.kind
            && tile.generation == self.generation
            && self
                .tiles
                .get(tile.slot as usize)
                .and_then(Option::as_ref)
                .is_some_and(|slot| slot.epoch == tile.slot_epoch)
    }

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

    pub(crate) fn clear(&mut self) {
        self.allocator = EtagereAllocator::new(size2(self.page.0 as i32, self.page.1 as i32));
        self.tiles.clear();
        self.free.clear();
        self.dirty.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    pub(crate) fn grow(&mut self) -> bool {
        let Some(next) = next_page(self.page.0, self.max_page) else {
            return false;
        };
        self.page = (next, next);
        self.clear();
        true
    }

    pub(crate) fn mark_written(&mut self, rect: DeviceRect) {
        self.dirty.mark(rect);
    }

    pub(crate) fn take_dirty_rects(&mut self) -> DirtyRects {
        core::mem::take(&mut self.dirty)
    }
}

pub(crate) struct ImageAtlasPager {
    pages: Vec<PagerPage>,
    idle: Vec<u16>,
    page_size: u32,
    clock: u64,
}

struct PagerPage {
    allocator: AtlasAllocator,
    live: usize,
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

    #[cfg(test)]
    pub(crate) fn with_page_for_test(page_size: u32) -> Self {
        Self::with_page_size(page_size)
    }

    pub(crate) fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub(crate) fn page_size(&self) -> u32 {
        self.page_size
    }

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
        self.idle.push(0);
        self.clock = 1;
    }

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
        for index in 0..self.pages.len() {
            if self.pages[index].live == 0 {
                continue;
            }
            if self.pages[index].allocator.has_room_for(content) {
                return false;
            }
        }
        if !self.idle.is_empty() {
            return false;
        }
        self.pages.len() < usize::from(MAX_IMAGE_PAGES)
    }

    pub(crate) fn allocate(&mut self, content: (u32, u32)) -> Option<Allocation> {
        let (width, height) = content;
        if width == 0 || height == 0 {
            return None;
        }
        let padded = (width.checked_add(2)?, height.checked_add(2)?);
        if padded.0 > self.page_size || padded.1 > self.page_size {
            return None;
        }
        for index in 0..self.pages.len() {
            if self.pages[index].live == 0 {
                continue;
            }
            if let Some(allocation) = self.allocate_on(index, content) {
                return Some(allocation);
            }
        }
        if let Some(idle_index) = self
            .idle
            .iter()
            .copied()
            .min_by_key(|page| self.pages[usize::from(*page)].last_used)
        {
            let index = usize::from(idle_index);
            let slot = &mut self.pages[index];
            slot.allocator.clear();
            slot.allocator.set_page_index(idle_index);
            slot.live = 0;
            self.idle.retain(|page| *page != idle_index);
            return self.allocate_on(index, content);
        }
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

    fn allocate_on(&mut self, index: usize, content: (u32, u32)) -> Option<Allocation> {
        let mut allocation = self.pages[index].allocator.allocate(content)?;
        allocation.tile.page = u16::try_from(index).ok()?;
        self.pages[index].live += 1;
        self.clock = self.clock.wrapping_add(1);
        self.pages[index].last_used = self.clock;
        Some(allocation)
    }

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

    pub(crate) fn mark_written(&mut self, page: u16, rect: DeviceRect) {
        if let Some(slot) = self.pages.get_mut(usize::from(page)) {
            slot.allocator.mark_written(rect);
        }
    }

    pub(crate) fn take_dirty_rects(&mut self, page: u16) -> Option<DirtyRects> {
        self.pages
            .get_mut(usize::from(page))
            .map(|slot| slot.allocator.take_dirty_rects())
    }

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

const FULL_RECT: DeviceRect = DeviceRect {
    x0: dp(0),
    y0: dp(0),
    x1: dp(i32::MAX),
    y1: dp(i32::MAX),
};

#[cfg(test)]
fn full_page(page: u32) -> DeviceRect {
    DeviceRect::new(dp(0), dp(0), dp(page as i32), dp(page as i32))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DirtyRects {
    rects: Vec<DeviceRect>,
}

impl DirtyRects {
    const MAX_RECTS: usize = 8;

    pub(crate) fn full() -> Self {
        Self {
            rects: vec![FULL_RECT],
        }
    }

    pub(crate) fn mark(&mut self, rect: DeviceRect) {
        if rect.is_empty() || self.is_full() {
            return;
        }
        if self.rects.iter().any(|existing| contains(*existing, rect)) {
            return;
        }
        let mut merged = rect;
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
            let mut union = self.rects[0];
            for rect in &self.rects[1..] {
                union = union.union(*rect);
            }
            self.rects.clear();
            self.rects.push(union);
        }
    }

    pub(crate) fn rects(&self) -> &[DeviceRect] {
        &self.rects
    }

    pub(crate) fn chunks(&self, page: u32, texel: u32) -> impl Iterator<Item = RectChunk> + '_ {
        let limit = page.min(i32::MAX as u32) as i32;
        let mut offset = 0usize;
        self.rects.iter().filter_map(move |rect| {
            let x0 = rect.x0.clamp(dp(0), dp(limit)).value() as u32;
            let y0 = rect.y0.clamp(dp(0), dp(limit)).value() as u32;
            let x1 = rect.x1.clamp(dp(0), dp(limit)).value() as u32;
            let y1 = rect.y1.clamp(dp(0), dp(limit)).value() as u32;
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

    pub(crate) fn byte_count(&self, page: u32, texel: u32) -> usize {
        self.chunks(page, texel)
            .map(|chunk| (chunk.width * chunk.height) as usize * texel as usize)
            .sum()
    }

    pub(crate) fn covers_page(&self, page: u32) -> bool {
        let page = page.min(i32::MAX as u32) as i32;
        self.rects
            .iter()
            .any(|rect| rect.x0 <= 0 && rect.y0 <= 0 && rect.x1 >= page && rect.y1 >= page)
    }

    pub(crate) fn is_full(&self) -> bool {
        self.rects.iter().any(|rect| {
            rect.x0 <= FULL_RECT.x0
                && rect.y0 <= FULL_RECT.y0
                && rect.x1 == FULL_RECT.x1
                && rect.y1 == FULL_RECT.y1
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    pub(crate) fn clear(&mut self) {
        self.rects.clear();
    }

    pub(crate) fn merge(&mut self, other: &mut Self) {
        let other = core::mem::take(other);
        for rect in other.rects {
            self.mark(rect);
        }
    }

    pub(crate) fn merge_from(&mut self, other: &Self) {
        for rect in &other.rects {
            self.mark(*rect);
        }
    }
}

fn contains(outer: DeviceRect, inner: DeviceRect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

fn touches(a: DeviceRect, b: DeviceRect) -> bool {
    a.x0 <= b.x1 && b.x0 <= a.x1 && a.y0 <= b.y1 && b.y0 <= a.y1
}

#[cfg(test)]
mod tests {
    use super::{
        ATLAS_PAGE_STEPS, AtlasAllocator, DirtyRects, ImageAtlasPager, MAX_ATLAS_PAGE,
        MIN_ATLAS_PAGE, RectChunk, pack_rects,
    };
    use crate::{
        math::{DeviceRect, dp},
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
            allocation.padded.width().value()
        );
        assert!(
            allocation.padded.height() >= 11,
            "padded height {} must cover content + 2",
            allocation.padded.height().value()
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
        atlas.deallocate(allocation.id);
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
    fn is_alive_tracks_allocation_eviction_slot_reuse_and_page_growth() {
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (64, 64));
        let first = atlas.allocate((8, 8)).expect("fits");
        assert!(atlas.is_alive(&first.tile), "刚分配的 tile 必须活着");
        assert_ne!(
            first.tile.slot_epoch, 0,
            "分配纪元从 1 起（0 = 从未分配的哨兵）"
        );

        atlas.deallocate(first.id);
        assert!(!atlas.is_alive(&first.tile), "释放后的 tile 必须失效");

        let reused = atlas.allocate((8, 8)).expect("释放出的槽位可复用");
        assert_eq!(
            reused.tile.slot, first.tile.slot,
            "64² 页只放得下一块 10×10 padded tile：必须复用同一槽位"
        );
        assert_ne!(
            reused.tile.slot_epoch, first.tile.slot_epoch,
            "复用槽位必须换分配纪元（否则旧 tile 会假活）"
        );
        assert!(atlas.is_alive(&reused.tile));
        assert!(
            !atlas.is_alive(&first.tile),
            "槽位复用后旧身份必须失效（generation 不变时的逐 tile 淘汰）"
        );

        assert!(atlas.grow());
        assert!(!atlas.is_alive(&reused.tile), "页扩容后旧代际 tile 失效");

        let mut fabricated = reused.tile;
        fabricated.slot = u32::MAX;
        fabricated.generation = atlas.generation();
        assert!(!atlas.is_alive(&fabricated), "越界槽位必须判死");

        let mut images = AtlasAllocator::with_page(AtlasKind::Image, (64, 64));
        let image_tile = images.allocate((8, 8)).expect("fits").tile;
        assert!(
            !atlas.is_alive(&image_tile),
            "字形分配器不得把图片 tile 判成活的"
        );
        assert!(images.is_alive(&image_tile));
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
    fn dirty_rects_merge_adjacent_and_ignore_contained_rects() {
        let mut rects = DirtyRects::default();
        rects.mark(DeviceRect::new(dp(0), dp(10), dp(8), dp(20)));
        rects.mark(DeviceRect::new(dp(8), dp(10), dp(16), dp(20)));
        rects.mark(DeviceRect::new(dp(0), dp(10), dp(16), dp(20)));
        rects.mark(DeviceRect::new(dp(100), dp(200), dp(108), dp(211)));
        assert_eq!(rects.rects().len(), 2);
        assert_eq!(
            rects.rects()[0],
            DeviceRect::new(dp(0), dp(10), dp(16), dp(20))
        );
        assert_eq!(
            rects.rects()[1],
            DeviceRect::new(dp(100), dp(200), dp(108), dp(211))
        );
        assert!(!rects.is_full());

        rects.mark(DeviceRect::new(dp(2), dp(12), dp(6), dp(18)));
        assert_eq!(rects.rects().len(), 2);
        assert_eq!(rects.byte_count(512, 1), 16 * 10 + 8 * 11);

        rects.mark(DeviceRect::new(dp(-4), dp(8), dp(4), dp(12)));
        assert_eq!(rects.rects().len(), 2);
        assert_eq!(
            rects.rects()[0],
            DeviceRect::new(dp(-4), dp(8), dp(16), dp(20))
        );
    }

    #[test]
    fn dirty_rects_fold_to_a_bounding_box_when_the_limit_is_exceeded_and_clear_empties() {
        let mut rects = DirtyRects::default();
        for index in 0..DirtyRects::MAX_RECTS {
            let y = index as i32 * 16;
            rects.mark(DeviceRect::new(dp(0), dp(y), dp(4), dp(y + 4)));
            assert_eq!(rects.rects().len(), index + 1, "第 {index} 块必须保留");
        }
        assert!(!rects.is_full());
        rects.mark(DeviceRect::new(dp(0), dp(4096), dp(4), dp(4100)));
        assert!(!rects.is_full(), "折叠后不得退化为整页哨兵");
        assert_eq!(
            rects.rects(),
            &[DeviceRect::new(dp(0), dp(0), dp(4), dp(4100))]
        );
        assert_eq!(rects.byte_count(512, 1), 4 * 512);

        rects.clear();
        assert!(rects.is_empty());
        assert!(rects.rects().is_empty());
    }

    #[test]
    fn nine_scattered_image_rects_fold_to_a_bounding_box_not_the_whole_page() {
        const PAGE: u32 = 2048;
        const TEXEL: u32 = 4;
        let mut rects = DirtyRects::default();
        let mut expected: Option<DeviceRect> = None;
        for index in 0..9i32 {
            let x = (index % 3) * 600;
            let y = (index / 3) * 600;
            let rect = DeviceRect::new(dp(x), dp(y), dp(x + 34), dp(y + 34));
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
        rects.mark(DeviceRect::new(dp(10), dp(10), dp(10), dp(20)));
        rects.mark(DeviceRect::new(dp(10), dp(20), dp(20), dp(20)));
        assert!(rects.is_empty());
        assert_eq!(rects.byte_count(512, 1), 0);
        assert_eq!(rects.chunks(512, 1).count(), 0);

        let page = vec![0u8; 512 * 512];
        let mut out = vec![7u8; 4];
        pack_rects(&page, 512, 1, &rects, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn merge_from_keeps_the_source_rects() {
        let mut source = DirtyRects::default();
        source.mark(DeviceRect::new(dp(0), dp(4), dp(8), dp(8)));
        let mut first = DirtyRects::default();
        let mut second = DirtyRects::default();
        first.merge_from(&source);
        second.merge_from(&source);

        assert_eq!(first.rects(), source.rects());
        assert_eq!(second.rects(), source.rects());
        assert_eq!(source.rects().len(), 1, "分发之后源必须仍然完整");
        assert!(!source.is_empty());

        first.merge(&mut source);
        assert!(source.is_empty(), "merge 必须消费源");
        assert_eq!(first.rects().len(), 1);
    }

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

    #[test]
    fn an_eight_by_eleven_glyph_uploads_88_bytes_instead_of_5632() {
        const PAGE: u32 = 512;
        let mut rects = DirtyRects::default();
        rects.mark(DeviceRect::new(dp(100), dp(200), dp(108), dp(211)));
        assert_eq!(rects.byte_count(PAGE, 1), 88, "AOI 字节数");
        let legacy = 11 * PAGE as usize;
        assert_eq!(legacy, 5632, "旧语义的行区间字节数");
        assert_eq!(legacy / rects.byte_count(PAGE, 1), 64, "放大倍数");
        assert_eq!(11 * 1024 / rects.byte_count(1024, 1), 128);
        assert_eq!(11 * 2048 / rects.byte_count(2048, 1), 256);

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

    #[test]
    fn a_sixteen_by_sixteen_image_uploads_1024_bytes_instead_of_32768() {
        const PAGE: u32 = 512;
        const TEXEL: u32 = 4;
        let mut rects = DirtyRects::default();
        rects.mark(DeviceRect::new(dp(64), dp(128), dp(80), dp(144)));
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
        assert_eq!(&out[..4], &[0x11, 0x22, 0x33, 0xFF]);
        assert_eq!(&out[16 * 4 - 4..16 * 4], &[0x11, 0x22, 0x33, 0xFF]);
    }

    #[test]
    fn multi_rect_packing_matches_the_chunk_offsets() {
        const PAGE: u32 = 64;
        let first = DeviceRect::new(dp(0), dp(10), dp(8), dp(12));
        let second = DeviceRect::new(dp(20), dp(10), dp(24), dp(13));
        let third = DeviceRect::new(dp(40), dp(40), dp(48), dp(41));
        let mut rects = DirtyRects::default();
        rects.mark(first);
        rects.mark(second);
        rects.mark(third);
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

        let expected_block = |rect: DeviceRect| -> Vec<u8> {
            let mut block =
                Vec::with_capacity(rect.width().value() as usize * rect.height().value() as usize);
            for y in rect.y0.value()..rect.y1.value() {
                for x in rect.x0.value()..rect.x1.value() {
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

    #[test]
    fn chunks_clamp_rects_to_the_page() {
        let mut rects = DirtyRects::default();
        rects.mark(DeviceRect::new(dp(-4), dp(-4), dp(6), dp(6)));
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
        off_page.mark(DeviceRect::new(dp(600), dp(600), dp(700), dp(700)));
        assert_eq!(off_page.byte_count(512, 1), 0, "页外矩形不上传");
        assert_eq!(off_page.chunks(512, 1).count(), 0);

        let mut crossing = DirtyRects::default();
        crossing.mark(DeviceRect::new(dp(500), dp(500), dp(700), dp(700)));
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

        let mut right_edge = DirtyRects::default();
        right_edge.mark(DeviceRect::new(dp(500), dp(10), dp(700), dp(14)));
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

        let mut bottom_edge = DirtyRects::default();
        bottom_edge.mark(DeviceRect::new(dp(10), dp(500), dp(14), dp(700)));
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
        let mut atlas = allocator();
        let allocation = atlas.allocate((8, 8)).expect("fits");
        atlas.deallocate(allocation.id);
        assert!(atlas.allocate((8, 8)).is_some(), "释放后容量必须回来");
    }

    #[test]
    fn image_pager_opens_a_second_page_when_the_first_is_full() {
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
        assert!(
            pager.allocate((62, 62)).is_none(),
            "活 tile 的页（0/3）绝不能被回收，且不得开第 5 页"
        );
    }

    #[test]
    fn the_image_array_layer_count_never_shrinks_when_an_idle_page_is_recycled() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let first = pager.allocate((62, 62)).expect("page 0 tile");
        let second = pager.allocate((62, 62)).expect("page 1 tile");
        assert_eq!(pager.page_count(), 2, "两块 62² tile 各占满一页");
        assert_ne!(first.tile.page, second.tile.page);

        pager.deallocate(first.tile.page, first.id);
        assert_eq!(
            pager.page_count(),
            2,
            "页变空不得减少页数（数组层数只增不减）"
        );

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

    #[test]
    fn image_pager_new_page_is_fully_dirty() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let _ = pager.allocate((18, 18)).expect("fits");
        let dirty = pager.take_dirty_rects(0).expect("page 0 exists");
        assert!(dirty.is_full(), "新页首次上传必须是整页");
        assert!(pager.take_dirty_rects(1).is_none(), "第 2 页还没开");
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

    #[test]
    fn image_pager_deallocate_is_idempotent() {
        let mut pager = ImageAtlasPager::with_page_for_test(64);
        let allocation = pager.allocate((18, 18)).expect("fits");
        pager.deallocate(0, allocation.id);
        pager.deallocate(0, allocation.id);
        pager.deallocate(99, allocation.id);
        let next = pager.allocate((18, 18)).expect("fits");
        assert_eq!(next.tile.page, 0);
    }

    #[test]
    fn covers_page_accepts_a_page_sized_rect_without_the_full_sentinel() {
        let page_sized = DirtyRects {
            rects: vec![DeviceRect::new(dp(0), dp(0), dp(64), dp(64))],
        };
        assert!(
            page_sized.covers_page(64),
            "恰好覆盖整页的矩形必须算整页（与 Windows/Linux 的 covers_full_page 同义）"
        );
        assert!(
            !page_sized.is_full(),
            "它不是 FULL_RECT 哨兵 —— 这正是 macOS 旧口径会误判的原因"
        );
        assert!(DirtyRects::full().covers_page(64), "整页哨兵也算覆盖整页");
        assert!(
            DirtyRects::full().covers_page(2048),
            "整页哨兵（i32::MAX 边）覆盖任意页尺寸"
        );
        assert!(
            !page_sized.covers_page(128),
            "64² 的矩形不覆盖 128² 页（按页尺寸判定，不是只看哨兵）"
        );
    }
}
