//! 图片页的 CPU 缓冲与条目记录（P13 §7）。
//!
//! 页缓冲是图片图集的**唯一**上传源：`Page` 恒为整页（`width * height * 4` 字节，BGRA8），
//! tile 内容在 `ResourceCache::insert` 时直接写进页（不为每条资源保存一份像素副本），
//! `unload` 时把该块连同 1 纹素 padding 一起清零。

use crate::{math::PhysicalRect, resources::ResourceImage};

/// 一条存活资源。
pub(crate) struct Entry {
    /// 交给应用的结果（句柄 + tile + 像素尺寸）。
    pub image: ResourceImage,
    /// `etagere` 的释放键（`Allocation::id`）。
    pub alloc: etagere::AllocId,
    /// 分配到的整块（含 padding；卸载时清零它）。
    pub padded: PhysicalRect,
}

/// 图片页的 CPU 缓冲（BGRA8 预乘 sRGB，行主序，行宽 `width * 4`）。
pub(crate) struct Page {
    width: u32,
    height: u32,
    bytes: Box<[u8]>,
}

impl Page {
    /// 新建一页（全 0：padding 与未使用区必须是透明的 0，P02 §10.3）。
    pub(crate) fn new(width: u32, height: u32) -> Self {
        assert!(width > 0 && height > 0, "atlas page must be non-empty");
        let len = width as usize * height as usize * 4;
        Self {
            width,
            height,
            bytes: vec![0u8; len].into_boxed_slice(),
        }
    }

    pub(crate) fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 把 `src`（每行 `stride` 字节，前 `rect.width() * 4` 有效）写进 `rect`（裁剪到页内）。
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

    /// 把 `rect` 清零（tile padding 与卸载区）。非法/空矩形是 no-op。
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

    /// 与页求交（半开区间；空交集返回 `None`）。
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
    use super::Page;
    use crate::math::PhysicalRect;

    fn page(width: u32, height: u32) -> Page {
        Page::new(width, height)
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
        // 2048² BGRA8（P02 §10.3）。
        let page = page(2048, 2048);
        assert_eq!(page.bytes().len(), 16 * 1024 * 1024);
    }

    #[test]
    fn write_then_read_back_the_same_rows() {
        let mut page = page(4, 2);
        let rect = PhysicalRect::new(1, 0, 3, 2);
        let source = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        page.write(rect, &source, 8);

        // 第一行：页偏移 (0*4 + 1)*4 = 4，写入 source[0..8]。
        assert_eq!(&page.bytes()[4..12], &source[..8]);
        // 第二行：页偏移 (1*4 + 1)*4 = 20，写入 source[8..16]。
        assert_eq!(&page.bytes()[20..28], &source[8..]);
        // 裁剪掉的第一列保持 0。
        assert_eq!(&page.bytes()[0..4], &[0; 4]);
        assert_eq!(&page.bytes()[16..20], &[0; 4]);
    }

    #[test]
    fn write_respects_a_padded_stride() {
        let mut page = page(2, 2);
        // 源行宽 16 B，但只有 8 B（2 像素）有效。
        let source = [9u8; 32];
        page.write(PhysicalRect::new(0, 0, 2, 2), &source, 16);
        assert_eq!(page.bytes(), &[9u8; 16]);
    }

    #[test]
    fn write_clips_out_of_bounds_rects() {
        let mut page = page(2, 2);
        // 右下越界：只写页内的 1×1。
        page.write(PhysicalRect::new(1, 1, 3, 3), &[7u8, 7, 7, 7], 4);
        let bytes = page.bytes();
        assert_eq!(&bytes[12..16], &[7; 4]);
        assert_eq!(&bytes[..12], &[0; 12]);

        // 完全在页外：no-op（不 panic）。
        page.write(PhysicalRect::new(-8, -8, -2, -2), &[1u8; 16], 8);
        assert_eq!(&page.bytes()[..12], &[0; 12]);
    }

    #[test]
    fn write_truncated_source_is_a_no_op_for_the_missing_rows() {
        let mut page = page(2, 2);
        // 声称 2 行但只给 1 行的字节：第一行写入，第二行因越界放弃。
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
}
