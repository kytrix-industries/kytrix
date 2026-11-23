use std::hash::Hasher;

use ahash::AHasher;
use bytes::Bytes;

use crate::{
    math::{PhysicalRect, Size},
    resources::{DecodeCause, ResourceId, ResourceImage, image::SourceKind},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContentKey {
    len: u32,
    hash: u64,
}

impl ContentKey {
    pub(crate) fn new(bytes: &[u8], kind: SourceKind) -> Self {
        let mut hasher = AHasher::default();
        hasher.write_u8(kind as u8);
        hasher.write(bytes);
        Self {
            len: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            hash: hasher.finish(),
        }
    }
}

pub(crate) struct DedupeEntry {
    pub bytes: Bytes,
    pub variants: Vec<VariantEntry>,
}

pub(crate) struct VariantEntry {
    pub target: (u32, u32),
    pub id: ResourceId,
}

pub(crate) struct Entry {
    pub image: ResourceImage,
    pub alloc: etagere::AllocId,
    pub padded: PhysicalRect,
    pub refcount: u32,
    pub last_used: u64,
    pub key: ContentKey,
    pub target: (u32, u32),
    pub intrinsic: (u32, u32),
    pub kind: SourceKind,
    pub logical: Option<Size>,
}

pub(crate) struct Page {
    width: u32,
    height: u32,
    bytes: Box<[u8]>,
}

impl Page {
    pub(crate) fn new(width: u32, height: u32) -> Result<Self, DecodeCause> {
        assert!(width > 0 && height > 0, "atlas page must be non-empty");
        let len = width as usize * height as usize * 4;
        let mut bytes: Vec<u8> = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| DecodeCause::OutOfMemory {
                limit_bytes: u64::try_from(len).unwrap_or(u64::MAX),
            })?;
        bytes.resize(len, 0);
        Ok(Self {
            width,
            height,
            bytes: bytes.into_boxed_slice(),
        })
    }

    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn write(&mut self, rect: PhysicalRect, src: &[u8], stride: usize) {
        let Some(bounds) = self.clip(rect) else {
            return;
        };
        let copy_width = (bounds.x1 - bounds.x0) as usize * 4;
        let source_x = (bounds.x0 - rect.x0) as usize * 4;
        let page_width = self.width as usize;
        for y in bounds.y0..bounds.y1 {
            let source_row = (y - rect.y0) as usize;
            let Some(source_start) = source_row
                .checked_mul(stride)
                .and_then(|row| row.checked_add(source_x))
            else {
                return;
            };
            let Some(source) = src.get(source_start..source_start + copy_width) else {
                return;
            };
            let destination = y as usize * page_width * 4 + bounds.x0 as usize * 4;
            self.bytes[destination..destination + copy_width].copy_from_slice(source);
        }
    }

    pub(crate) fn clear_rect(&mut self, rect: PhysicalRect) {
        let Some(bounds) = self.clip(rect) else {
            return;
        };
        let row_bytes = (bounds.x1 - bounds.x0) as usize * 4;
        let page_width = self.width as usize;
        for y in bounds.y0..bounds.y1 {
            let start = y as usize * page_width * 4 + bounds.x0 as usize * 4;
            self.bytes[start..start + row_bytes].fill(0);
        }
    }

    fn clip(&self, rect: PhysicalRect) -> Option<PhysicalRect> {
        let x0 = rect.x0.max(0).min(self.width as i32);
        let x1 = rect.x1.max(0).min(self.width as i32);
        let y0 = rect.y0.max(0).min(self.height as i32);
        let y1 = rect.y1.max(0).min(self.height as i32);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some(PhysicalRect::new(x0, y0, x1, y1))
    }
}

#[cfg(test)]
mod tests {
    use super::{ContentKey, Page};
    use crate::{
        math::PhysicalRect,
        resources::{DecodeCause, image::SourceKind},
    };

    fn page(width: u32, height: u32) -> Page {
        Page::new(width, height).expect("测试页缓冲分配（小页，不应失败）")
    }

    #[test]
    fn page_allocation_failure_maps_to_out_of_memory() {
        let error = Page::new(1_000_000_000, 1_000_000_000)
            .err()
            .expect("荒谬大的页必须分配失败");
        assert!(
            matches!(error, DecodeCause::OutOfMemory { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn new_page_is_zeroed_with_the_expected_length() {
        let page = page(4, 3);
        assert_eq!(page.dimensions(), (4, 3));
        assert_eq!(page.bytes().len(), 4 * 3 * 4);
        assert!(page.bytes().iter().all(|byte| *byte == 0));
    }

    #[test]
    fn default_image_page_is_16_mib() {
        let page = page(2048, 2048);
        assert_eq!(page.bytes().len(), 16 * 1024 * 1024);
    }

    #[test]
    fn write_then_read_back_the_same_rows() {
        let mut page = page(4, 2);
        let rect = PhysicalRect::new(1, 0, 3, 2);
        let source = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        page.write(rect, &source, 8);

        assert_eq!(&page.bytes()[4..12], &source[..8]);
        assert_eq!(&page.bytes()[20..28], &source[8..]);
        assert_eq!(&page.bytes()[0..4], &[0; 4]);
        assert_eq!(&page.bytes()[16..20], &[0; 4]);
    }

    #[test]
    fn write_respects_a_padded_stride() {
        let mut page = page(2, 2);
        let source = [9u8; 32];
        page.write(PhysicalRect::new(0, 0, 2, 2), &source, 16);
        assert_eq!(page.bytes(), &[9u8; 16]);
    }

    #[test]
    fn write_clips_out_of_bounds_rects() {
        let mut page = page(2, 2);
        page.write(PhysicalRect::new(1, 1, 3, 3), &[7u8, 7, 7, 7], 4);
        let bytes = page.bytes();
        assert_eq!(&bytes[12..16], &[7; 4]);
        assert_eq!(&bytes[..12], &[0; 12]);

        page.write(PhysicalRect::new(-8, -8, -2, -2), &[1u8; 16], 8);
        assert_eq!(&page.bytes()[..12], &[0; 12]);
    }

    #[test]
    fn write_truncated_source_is_a_no_op_for_the_missing_rows() {
        let mut page = page(2, 2);
        page.write(PhysicalRect::new(0, 0, 2, 2), &[3u8; 8], 8);
        assert_eq!(&page.bytes()[..8], &[3u8; 8]);
        assert_eq!(&page.bytes()[8..], &[0u8; 8]);
    }

    #[test]
    fn clear_rect_zeroes_only_that_rect() {
        let mut page = page(2, 2);
        page.write(PhysicalRect::new(0, 0, 2, 2), &[5u8; 16], 8);
        page.clear_rect(PhysicalRect::new(1, 0, 2, 1));
        assert_eq!(&page.bytes()[..4], &[5u8; 4]);
        assert_eq!(&page.bytes()[4..8], &[0u8; 4]);
        assert_eq!(&page.bytes()[8..], &[5u8; 8]);
    }

    #[test]
    fn clear_rect_ignores_empty_rects() {
        let mut page = page(2, 2);
        page.write(PhysicalRect::new(0, 0, 2, 2), &[6u8; 16], 8);
        page.clear_rect(PhysicalRect::new(1, 1, 1, 1));
        assert_eq!(page.bytes(), &[6u8; 16]);
    }

    #[test]
    fn content_key_is_stable_for_equal_bytes_and_splits_different_lengths() {
        let first = ContentKey::new(b"logo", SourceKind::Bitmap);
        let second = ContentKey::new(b"logo", SourceKind::Bitmap);
        let longer = ContentKey::new(b"logos", SourceKind::Bitmap);

        assert_eq!(first, second, "同一份字节的内容键必须相同");
        assert_ne!(first, longer, "长度进键：不同长度不该互相命中");
        assert_ne!(
            ContentKey::new(b"logo", SourceKind::Bitmap),
            ContentKey::new(b"logo", SourceKind::Svg),
            "来源类型进键：同一份字节不能跨 PNG/SVG 复用变体"
        );
        assert_ne!(
            ContentKey::new(b"logo", SourceKind::Bitmap),
            ContentKey::new(b"olgo", SourceKind::Bitmap)
        );
    }
}
