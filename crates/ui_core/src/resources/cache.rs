//! 图片页的 CPU 缓冲与缓存索引（P13 §7，D-10 两级变体缓存）。
//!
//! 页缓冲是图片图集的**唯一**上传源：`Page` 恒为整页（`width * height * 4` 字节，BGRA8），
//! tile 内容在 `ResourceCache::place` 时直接写进页（不为每条资源保存一份像素副本），
//! 引用计数归零（`unload` / `release_window`）时把该块连同 1 纹素 padding 一起清零。
//!
//! 两级索引（D-10；D-10 之前是「内容 → 单 entry」）：
//! - [`ContentKey`] / [`DedupeEntry`]：内容的哈希索引 + 原始字节 + 该内容的**全部存活变体**；
//!   命中去重时按 `(bytes, target)` 匹配变体、**不重新解码**，源字节只保留一份；
//! - [`Entry`]：存活资源（变体），一条 = 一个 tile + 一个变体键 + 引用计数 + LRU epoch。
//!
//! 同一内容的「原始尺寸」与「目标尺寸」变体、以及不同目标尺寸的变体可以同时存在
//! （例如列表缩略图与大图预览共用同一份压缩字节）；全部变体的引用计数归零后，
//! [`DedupeEntry`] 才被删除、源字节随之释放。

use std::hash::Hasher;

use ahash::AHasher;
use bytes::Bytes;

use crate::{
    math::PhysicalRect,
    resources::{DecodeCause, ResourceId, ResourceImage},
};

/// 图片内容的哈希键：`(len, 64 bit 哈希)`。
///
/// 哈希本身**不能**证明相等：命中后调用方必须用原始字节 memcmp（见 [`DedupeEntry::bytes`]）。
/// `len` 在键里，长度不同的字节连 memcmp 都不用做。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContentKey {
    len: u32,
    hash: u64,
}

impl ContentKey {
    /// 对一份字节做内容键（`ahash`，固定 keys；只用于索引，不用于任何安全判定）。
    pub(crate) fn new(bytes: &[u8]) -> Self {
        let mut hasher = AHasher::default();
        hasher.write(bytes);
        Self {
            len: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
            hash: hasher.finish(),
        }
    }
}

/// 去重表的一条：内容 → 该内容的全部存活变体 + 原始字节（memcmp 用）。
pub(crate) struct DedupeEntry {
    /// 原始输入字节（`Bytes`：`insert_async` 路径零拷贝共享）。**同一内容只保留一份**，
    /// 全部变体回收后才删除本条目并释放它。
    pub bytes: Bytes,
    /// 该内容的存活变体（同一字节不同目标尺寸并存；元素数 ≥ 1）。
    pub variants: Vec<VariantEntry>,
}

/// 去重表里的一个内容变体：变体键（目标尺寸）→ 资源句柄。
pub(crate) struct VariantEntry {
    /// 变体键：请求的物理目标尺寸（`ceil` 后 clamp 到源尺寸与图片页单块上限 `MAX_TILE_DIM`）。
    /// `(0, 0)` 保留给「原始尺寸」变体（`resources::ORIGINAL_TARGET`）。
    pub target: (u32, u32),
    /// 该变体的资源句柄。
    pub id: ResourceId,
}

/// 一条存活资源（一个变体）。
pub(crate) struct Entry {
    /// 交给应用的结果（句柄 + tile + 像素尺寸）。
    pub image: ResourceImage,
    /// `etagere` 的释放键（`Allocation::id`）。
    pub alloc: etagere::AllocId,
    /// 分配到的整块（含 padding；卸载时清零它）。
    pub padded: PhysicalRect,
    /// 引用计数：同一变体每被引用一次 +1，归零才回收 tile。
    pub refcount: u32,
    /// 最近一次被引用的 LRU epoch（`ResourceCache::release_window` 的回收顺序）。
    pub last_used: u64,
    /// 内容键（回收时用它从去重表删去本变体）。
    pub key: ContentKey,
    /// 变体键（与 [`VariantEntry::target`] 一致；回收时按 `(key, target, id)` 精确删变体）。
    pub target: (u32, u32),
}

/// 图片页的 CPU 缓冲（BGRA8 预乘 sRGB，行主序，行宽 `width * 4`）。
pub(crate) struct Page {
    width: u32,
    height: u32,
    bytes: Box<[u8]>,
}

impl Page {
    /// 新建一页（全 0：padding 与未使用区必须是透明的 0，P02 §10.3）。
    ///
    /// RD-2：16 MiB 级缓冲属于**引擎自有**分配，必须走 `try_reserve_exact` 并把失败映射为
    /// [`DecodeCause::OutOfMemory`]（release `panic=abort` 下 `vec!` 分配失败会直接杀进程）。
    /// 调用方（`ResourceCache::place`）在失败时负责回滚已经拿到的图集分配。
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
    use super::{ContentKey, Page};
    use crate::{math::PhysicalRect, resources::DecodeCause};

    fn page(width: u32, height: u32) -> Page {
        Page::new(width, height).expect("测试页缓冲分配（小页，不应失败）")
    }

    /// RD-2：页缓冲分配失败必须映射为 `DecodeCause::OutOfMemory`，而不是 panic/abort。
    #[test]
    fn page_allocation_failure_maps_to_out_of_memory() {
        // 4.0e18 字节的请求必然 `try_reserve_exact` 失败（不会真分配）。
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

    #[test]
    fn content_key_is_stable_for_equal_bytes_and_splits_different_lengths() {
        let first = ContentKey::new(b"logo");
        let second = ContentKey::new(b"logo");
        let longer = ContentKey::new(b"logos");

        assert_eq!(first, second, "同一份字节的内容键必须相同");
        assert_ne!(first, longer, "长度进键：不同长度不该互相命中");
        assert_ne!(ContentKey::new(b"logo"), ContentKey::new(b"olgo"));
    }
}
