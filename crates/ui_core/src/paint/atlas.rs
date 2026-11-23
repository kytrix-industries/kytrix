pub const MAX_IMAGE_PAGES: u16 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AtlasKind {
    Glyph = 0,
    Image = 1,
}

impl AtlasKind {
    #[must_use]
    pub const fn index(self) -> u32 {
        self as u32
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasTile {
    pub atlas: AtlasKind,
    pub page: u16,
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    pub generation: u32,
    pub slot: u32,
    pub slot_epoch: u32,
}

impl AtlasTile {
    #[must_use]
    pub const fn uv(self) -> [f32; 4] {
        [self.u0, self.v0, self.u1, self.v1]
    }

    #[must_use]
    pub const fn page_f32(self) -> f32 {
        self.page as f32
    }

    #[must_use]
    pub const fn is_stale(self, atlas_generation: u32) -> bool {
        self.generation != atlas_generation
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::{AtlasKind, AtlasTile, MAX_IMAGE_PAGES};

    fn tile(
        atlas: AtlasKind,
        page: u16,
        u0: f32,
        v0: f32,
        u1: f32,
        v1: f32,
        generation: u32,
    ) -> AtlasTile {
        AtlasTile {
            atlas,
            page,
            u0,
            v0,
            u1,
            v1,
            generation,
            slot: 0,
            slot_epoch: 0,
        }
    }

    #[test]
    fn atlas_kind_index_matches_the_backend_slots() {
        assert_eq!(AtlasKind::Glyph.index(), 0);
        assert_eq!(AtlasKind::Image.index(), 1);
    }

    #[test]
    fn max_image_pages_is_an_upper_bound_not_an_array_layer_count() {
        assert_eq!(MAX_IMAGE_PAGES, 4, "契约上限：4 页 = 约 14.5k 个 32² tile");
        let per_layer_bytes = 2048u64 * 2048 * 4;
        assert_eq!(
            per_layer_bytes * u64::from(MAX_IMAGE_PAGES),
            64 * 1024 * 1024,
            "旧实现的常驻量：每窗口 4 层 × 16 MiB"
        );
        assert_eq!(per_layer_bytes, 16 * 1024 * 1024, "单页/单层 = 16 MiB");
    }

    #[test]
    fn tile_uv_order() {
        let t = tile(AtlasKind::Glyph, 0, 0.25, 0.5, 0.75, 1.0, 3);
        assert_eq!(t.uv(), [0.25, 0.5, 0.75, 1.0]);
        assert!(t.u0 < t.u1);
        assert!(t.v0 < t.v1);
    }

    #[test]
    fn tile_staleness() {
        let t = tile(AtlasKind::Image, 0, 0.0, 0.0, 0.5, 0.5, 7);
        assert!(!t.is_stale(7));
        assert!(t.is_stale(8));
    }

    #[test]
    fn tile_page_encodes_as_a_small_integer_float() {
        for page in 0..MAX_IMAGE_PAGES {
            let t = tile(AtlasKind::Image, page, 0.0, 0.0, 1.0, 1.0, 1);
            assert_eq!(t.page_f32(), f32::from(page));
            assert_eq!(t.page_f32() as u32, u32::from(page));
        }
        assert_eq!(MAX_IMAGE_PAGES, 4, "契约上限：4 页 = 约 14.5k 个 32² tile");
    }

    #[test]
    fn size_budget() {
        assert_eq!(size_of::<AtlasKind>(), 1);
        assert_eq!(size_of::<AtlasTile>(), 32);
        assert_eq!(
            size_of::<AtlasTile>() - 24,
            8,
            "相对 24 B 版本的增量（内存影响见 spec-delta §7.4）"
        );
    }
}
