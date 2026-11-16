//! 图片资源缓存（P13）：字节 → 解码 → 图片图集 tile + 页缓冲。
//!
//! 公开面只有 [`ResourceId`] / [`ResourceImage`] / [`DecodeCause`] / [`ResourceCache`]；
//! 图集的**字节语义**（线性预乘 → sRGB 编码的 BGRA8）来自 `render/shaders/contract.md` §8，
//! `AtlasTile` / `AtlasKind` 的归属在 `paint::atlas`（DEVIATIONS F-24）。
//!
//! **三条硬规则**（`design/phase-13-resources-snapshot.md` §5）：
//!
//! 1. tile 一旦交给应用就**恒有效**，直到 `unload(id)`——引擎不会自动淘汰（`Canvas` 是值语义快照，
//!    失效 tile 会让旧图元画出别人的图）；容量不足是强类型 [`DecodeCause::Capacity`]。
//! 2. 不做远程磁盘缓存、不碰 `usvg`：输入只有 `&[u8]`，字节来源归应用层。
//! 3. 失败是强类型 [`DecodeCause`]，不返回字符串错误、不包装 `image::ImageError`。

use std::{fmt, num::NonZeroU32};

use crate::{
    error::EngineError,
    math::PhysicalRect,
    node::id::{Arena, ArenaKey},
    paint::atlas::{AtlasKind, AtlasTile},
    render::{AtlasAllocator, DirtyRows},
    resources::cache::{Entry, Page},
};

mod cache;
mod image;

/// 图片资源句柄（8 B，带代际）。
///
/// 字段私有：只能由 [`ResourceCache::insert`] 铸出（`NodeId` 的同类纪律，P02 S-20）。
/// `unload` 释放槽位时代际 `+1`，因此旧句柄**永不复活**。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId {
    index: u32,
    generation: NonZeroU32,
}

impl ResourceId {
    /// 槽位下标。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// 代际，恒在 `1..=u32::MAX`（0 永不是合法代际）。
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    /// `(generation as u64) << 32 | index as u64`（结构化日志 / 帧记录的单值标识）。
    #[must_use]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    /// 仅测试用的构造器（不扩大公开面）。
    #[cfg(test)]
    pub(crate) const fn for_test(index: u32, generation: u32) -> Self {
        match NonZeroU32::new(generation) {
            Some(generation) => Self { index, generation },
            None => panic!("ResourceId 的代际不得为 0"),
        }
    }
}

impl fmt::Debug for ResourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ResourceId(r{}.{})", self.index, self.generation)
    }
}

impl ArenaKey for ResourceId {
    fn from_parts(index: u32, generation: u32) -> Self {
        let generation =
            NonZeroU32::new(generation).expect("arena 的代际恒在 1..=u32::MAX（引擎不变量）");
        Self { index, generation }
    }

    fn parts(self) -> (u32, u32) {
        (self.index, self.generation.get())
    }
}

/// 一次成功插入的结果（`Copy`；应用层自己保存，引擎不回查）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResourceImage {
    /// 该资源的句柄（`unload` 的唯一入口）。
    pub id: ResourceId,
    /// 可直接塞进 `Primitive::Image` / `Canvas::push_image` 的图集矩形。
    pub tile: AtlasTile,
    /// 图集内容区像素宽（缩放后的最终尺寸，`1..=2046`）。
    pub width: u32,
    /// 图集内容区像素高。
    pub height: u32,
}

/// 解码 / 装箱失败的业务分类（[`EngineError::ResourceDecode`] 的 payload）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeCause {
    /// 无法识别的格式，或不是 PNG / JPEG / WebP。
    UnsupportedFormat {
        /// 诊断用的稳定格式名（`"gif"` / `"png"` / `"unknown"` …）。
        format: smol_str::SmolStr,
    },
    /// 字节无法产生有效像素（表单自洽性失败、解码器内存上限、零尺寸）。
    Corrupt {
        /// 诊断用的稳定格式名。
        format: smol_str::SmolStr,
        /// `image` 的原始错误文本（只用于日志）。
        detail: smol_str::SmolStr,
    },
    /// 源图单边超出上限（`MAX_SOURCE_DIM`）；`width` / `height` 为实际值。
    TooLarge {
        /// 实际宽。
        width: u32,
        /// 实际高。
        height: u32,
        /// 单边上限。
        limit: u32,
    },
    /// 图片页放不下该 tile（`unload` 其它资源后重试）。
    Capacity {
        /// 请求的内容区宽。
        width: u32,
        /// 请求的内容区高。
        height: u32,
        /// 图片页尺寸（纹素）。
        page: (u32, u32),
    },
}

impl fmt::Display for DecodeCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat { format } => write!(
                f,
                "unsupported image format `{format}` (PNG, JPEG and WebP only)"
            ),
            Self::Corrupt { format, detail } => {
                write!(f, "corrupt `{format}` bytes: {detail}")
            }
            Self::TooLarge {
                width,
                height,
                limit,
            } => write!(f, "image is {width}x{height}, limit is {limit} px per side"),
            Self::Capacity {
                width,
                height,
                page,
            } => write!(
                f,
                "a {width}x{height} tile does not fit the {}x{} image atlas: the page is already \
                 full (unload resources you no longer draw to free space)",
                page.0, page.1
            ),
        }
    }
}

/// 图片资源缓存：字节 → 解码 → 图片图集 tile + 页缓冲。
///
/// **不持有 GPU 对象**：页缓冲由 `App::upload_image_page` 交给窗口渲染器（与字形页同一路径）。
/// 进程级一份（单 GPU 设备），由 `App` 持有（`design/phase-13-resources-snapshot.md` §8）。
pub struct ResourceCache {
    entries: Arena<Entry, ResourceId>,
    /// 图片页 + 分配器：**固定用最大档**（`MAX_ATLAS_PAGE` = 2048，见 `insert` 里的说明），
    /// 因此容纳能力与第一张图片的尺寸无关。
    ///
    /// 没有插入过图片时不分配 CPU 页（16 MiB 量级）、GPU 纹理与 staging。
    page: Option<PageState>,
}

/// 图片页 + 它的分配器（同生同灭：页尺寸锁定后不再变化）。
struct PageState {
    page: Page,
    allocator: AtlasAllocator,
}

impl ResourceCache {
    /// 新建：图片页在首次 `insert` 时才分配。
    pub(crate) fn new() -> Self {
        Self {
            entries: Arena::new(),
            page: None,
        }
    }

    /// 仅测试：用指定页尺寸的小页构造（跳过选档，便于穷尽容量路径）。
    #[cfg(test)]
    pub(crate) fn with_page_for_test(page: (u32, u32)) -> Self {
        let mut cache = Self::new();
        cache.open_page(page.0);
        cache
    }

    /// 建页（首次插入 / 测试注入）：整页先标记为脏，保证 GPU 侧新建纹理的其余区域是 0。
    fn open_page(&mut self, page: u32) {
        debug_assert!(self.page.is_none(), "图片页只建一次");
        let mut allocator = AtlasAllocator::with_page(AtlasKind::Image, (page, page));
        allocator.mark_written(full_page((page, page)));
        self.page = Some(PageState {
            page: Page::new(page, page),
            allocator,
        });
    }

    /// 解码 `bytes` 并装箱，返回句柄、tile 与像素尺寸。
    ///
    /// # Errors
    /// [`EngineError::ResourceDecode`]：`UnsupportedFormat` / `Corrupt` / `TooLarge` / `Capacity`。
    pub fn insert(&mut self, bytes: &[u8]) -> Result<ResourceImage, EngineError> {
        let decoded =
            image::decode(bytes).map_err(|cause| EngineError::ResourceDecode { cause })?;
        let (width, height) = (decoded.width, decoded.height);
        // **图片页固定开最大档**（P1-2.7）。旧实现用 `page_for_tile(width, height)` 按**第一张**
        // 图片选档并永久锁定：先加载 16×16 图标 ⇒ 页 = 512² ⇒ 之后任何 >510 px 的图必然
        // `Capacity`，而错误文案还让「unload 后重试」（页尺寸永不变，重试无效）。
        // 加载顺序决定整个进程的图片上限是不可接受的隐式契约。
        //
        // 固定最大档后，容纳能力与插入顺序无关；懒分配语义不变（没有图片就仍不占内存），
        // 代价是需要图片时一次性占 16 MiB —— 这本来就是文档里的图片页上限。
        // 更省内存的做法是 Skia/Impeller 式的多页图集（`AtlasTile` 增 `page`、三平台
        // `texture_index` 改纹理数组），属契约变更，留作后续优化。
        if self.page.is_none() {
            self.open_page(crate::render::atlas::MAX_ATLAS_PAGE);
        }
        let state = self.page.as_mut().expect("page was just ensured");
        let Some(allocation) = state.allocator.allocate((width, height)) else {
            return Err(EngineError::ResourceDecode {
                cause: DecodeCause::Capacity {
                    width,
                    height,
                    page: state.page.dimensions(),
                },
            });
        };
        debug_assert_eq!(allocation.tile.atlas, AtlasKind::Image);
        // padding 复用时可能残留上一块的内容：先清整块（含 padding），再写内容区。
        state.page.clear_rect(allocation.padded);
        state.allocator.mark_written(allocation.padded);
        state
            .page
            .write(allocation.content, &decoded.bgra, width as usize * 4);
        let id = self.entries.insert_with(|id| Entry {
            image: ResourceImage {
                id,
                tile: allocation.tile,
                width,
                height,
            },
            alloc: allocation.id,
            padded: allocation.padded,
        });
        Ok(self
            .entries
            .get(id)
            .expect("just inserted entry must exist")
            .image)
    }

    /// 释放一个资源：tile 归还分配器、页内区域（含 padding）清零。
    ///
    /// **契约**：调用方必须保证不再有 `Image` 图元引用该 tile（通常是先卸载/重建对应节点）。
    /// 误用时该区域被清零（表现为透明），若区域随后被新资源复用则旧图元会采样到新图。
    /// 返回 `false` = 陈旧句柄（幂等）。
    pub fn unload(&mut self, id: ResourceId) -> bool {
        let Some(entry) = self.entries.remove(id) else {
            return false;
        };
        let Some(state) = self.page.as_mut() else {
            return true;
        };
        state.allocator.deallocate(entry.alloc);
        state.page.clear_rect(entry.padded);
        state.allocator.mark_written(entry.padded);
        true
    }

    /// 存活资源数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空（快照路径的上传开关）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 图片页的当前内容（`page.0 * page.1 * 4` 字节，BGRA8 预乘 sRGB）；没插过图片时 `None`。
    pub(crate) fn page_pixels(&self) -> Option<((u32, u32), &[u8])> {
        self.page
            .as_ref()
            .map(|state| (state.page.dimensions(), state.page.bytes()))
    }

    /// 取走自上次上传以来被写脏的**行**（首次调用返回整页）；没有图片页时是空集。
    pub(crate) fn take_dirty_rows(&mut self) -> DirtyRows {
        self.page.as_mut().map_or_else(DirtyRows::default, |state| {
            state.allocator.take_dirty_rows()
        })
    }
}

/// 整页矩形（`mark_written` 的入参）。
fn full_page(page: (u32, u32)) -> PhysicalRect {
    PhysicalRect::new(0, 0, page.0 as i32, page.1 as i32)
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::{DecodeCause, ResourceCache, ResourceId, ResourceImage};
    use crate::{error::EngineError, paint::atlas::AtlasKind};

    fn png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let pixels: Vec<u8> = std::iter::repeat_n(rgba, (width * height) as usize)
            .flatten()
            .collect();
        let image = image::RgbaImage::from_raw(width, height, pixels).expect("raw");
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    fn cache() -> ResourceCache {
        // 16×16 的页：只放得下一块 14×14 的内容（+2 padding）。
        ResourceCache::with_page_for_test((16, 16))
    }

    fn inserted(cache: &mut ResourceCache, rgba: [u8; 4]) -> ResourceImage {
        cache.insert(&png(4, 4, rgba)).expect("insert 4x4 png")
    }

    fn insert_error(cache: &mut ResourceCache, bytes: &[u8]) -> EngineError {
        cache.insert(bytes).expect_err("must fail")
    }

    fn capacity_cause(error: EngineError) -> DecodeCause {
        match error {
            EngineError::ResourceDecode { cause } => cause,
            other => panic!("expected ResourceDecode, got {other:?}"),
        }
    }

    #[test]
    fn resource_id_is_8_bytes() {
        assert_eq!(size_of::<ResourceId>(), 8);
        assert_eq!(size_of::<Option<ResourceId>>(), 8);
    }

    #[test]
    fn resource_id_to_bits_layout() {
        let id = ResourceId::for_test(3, 7);
        assert_eq!(id.to_bits(), (7u64 << 32) | 3);
        assert_eq!(id.index(), 3);
        assert_eq!(id.generation(), 7);
        assert_eq!(format!("{id:?}"), "ResourceId(r3.7)");
    }

    #[test]
    fn insert_returns_a_tile_per_call() {
        let mut cache = cache();
        let first = inserted(&mut cache, [1, 2, 3, 255]);
        let second = inserted(&mut cache, [4, 5, 6, 255]);

        assert_ne!(first.id, second.id, "每次插入铸出独立句柄");
        assert_ne!(
            first.tile.uv(),
            second.tile.uv(),
            "不做按内容去重（两次插入两块 tile）"
        );
        assert_eq!(cache.len(), 2);
        assert!(!cache.is_empty());
    }

    #[test]
    fn insert_png_happy_path() {
        let mut cache = super::ResourceCache::new();
        let image = inserted(&mut cache, [10, 20, 30, 128]);

        assert_eq!((image.width, image.height), (4, 4));
        assert_eq!(image.tile.atlas, AtlasKind::Image);
        assert_eq!(image.tile.generation, 1);
        assert!(image.tile.u0 < image.tile.u1 && image.tile.v0 < image.tile.v1);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn insert_rejects_unsupported_and_unknown_formats() {
        let mut cache = cache();
        assert_eq!(
            capacity_cause(insert_error(
                &mut cache,
                b"GIF89a\x01\x00\x01\x00\x00\x00\x00"
            )),
            DecodeCause::UnsupportedFormat {
                format: smol_str::SmolStr::new_static("gif")
            }
        );
        assert_eq!(
            capacity_cause(insert_error(&mut cache, &[0u8; 8])),
            DecodeCause::UnsupportedFormat {
                format: smol_str::SmolStr::new_static("unknown")
            }
        );
        assert_eq!(cache.len(), 0, "失败的插入不得留下条目");
    }

    #[test]
    fn insert_rejects_corrupt_png() {
        let mut cache = cache();
        let png = png(4, 4, [1, 2, 3, 255]);
        let error = capacity_cause(insert_error(&mut cache, &png[..png.len() / 2]));
        assert!(
            matches!(&error, DecodeCause::Corrupt { format, .. } if format == "png"),
            "{error:?}"
        );
        assert_eq!(cache.len(), 0);
    }

    /// P1-2.7 回归：图片页的容纳能力**不得**依赖插入顺序。
    ///
    /// 旧实现按第一张图选档并永久锁定：先插 16×16 图标 ⇒ 页 = 512² ⇒ 之后 600×600 必然
    /// `Capacity`，且 `unload` 后重试也无效（页尺寸永不变）。
    #[test]
    fn page_capacity_is_independent_of_insertion_order() {
        let mut cache = ResourceCache::new();
        // 先一张小图（旧实现会把页钉在 512²）。
        let tiny = inserted(&mut cache, [1, 2, 3, 255]);
        assert_eq!((tiny.width, tiny.height), (4, 4));
        // 再一张远超 512 的图：必须能放下。
        let big = cache.insert(&png(600, 600, [9, 9, 9, 255])).expect(
            "600x600 must fit after a tiny image (the page is not locked by the first insert)",
        );
        assert_eq!((big.width, big.height), (600, 600));
        // 两张都还活着，且指向同一页。
        assert_eq!(tiny.tile.atlas, big.tile.atlas);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn insert_reports_capacity_with_sizes() {
        // 8×8 的页：一块 4×4（+2 padding = 6×6）之后放不下第二块。
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let _ = inserted(&mut cache, [7, 7, 7, 255]);
        let error = capacity_cause(insert_error(&mut cache, &png(4, 4, [8, 8, 8, 255])));
        assert_eq!(
            error,
            DecodeCause::Capacity {
                width: 4,
                height: 4,
                page: (8, 8)
            }
        );
        assert_eq!(cache.len(), 1, "容量失败不改变既有条目");
    }

    #[test]
    fn capacity_failure_does_not_bump_generation() {
        let mut cache = ResourceCache::with_page_for_test((8, 8));
        let image = inserted(&mut cache, [9, 9, 9, 255]);
        let generation = image.tile.generation;
        let _ = insert_error(&mut cache, &png(4, 4, [1, 1, 1, 255]));

        assert_eq!(
            cache
                .entries
                .get(image.id)
                .expect("entry survives")
                .image
                .tile,
            image.tile
        );
        assert!(!image.tile.is_stale(generation), "容量失败不换代（F-140）");
    }

    #[test]
    fn unload_is_idempotent_and_stale_handles_stay_dead() {
        let mut cache = cache();
        let image = inserted(&mut cache, [1, 2, 3, 255]);

        assert!(cache.unload(image.id));
        assert!(!cache.unload(image.id), "重复卸载是 no-op");
        assert_eq!(cache.len(), 0);

        // 槽位复用后代际不同 ⇒ 旧句柄仍然无效。
        let replacement = inserted(&mut cache, [4, 5, 6, 255]);
        assert_eq!(replacement.id.index(), image.id.index(), "槽位被复用");
        assert_ne!(replacement.id.generation(), image.id.generation());
        assert!(!cache.unload(image.id));
    }

    #[test]
    fn unload_clears_the_page_region_and_marks_rows_dirty() {
        let mut cache = cache();
        let image = inserted(&mut cache, [200, 100, 50, 255]);
        let _ = cache.take_dirty_rows(); // 首帧已上传整页
        let before: Vec<u8> = cache.page_pixels().expect("page").1.to_vec();
        assert!(before.iter().any(|byte| *byte != 0), "插入必须写进页");

        assert!(cache.unload(image.id));
        let after = cache.page_pixels().expect("page").1;
        assert!(after.iter().all(|byte| *byte == 0), "卸载后区域必须清零");
        let rows = cache.take_dirty_rows();
        assert!(!rows.spans().is_empty(), "清零区必须上报脏行");
    }

    #[test]
    fn first_dirty_rows_cover_the_whole_page() {
        let mut cache = cache();
        let rows = cache.take_dirty_rows();
        assert_eq!(rows.spans().len(), 1);
        assert_eq!(
            rows.spans()[0],
            0..16,
            "首次上传必须是整页（GPU 侧内容未定义）"
        );
        assert!(cache.take_dirty_rows().spans().is_empty(), "take 会清空");
    }

    #[test]
    fn insert_marks_only_the_allocated_rows_dirty() {
        let mut cache = cache();
        let _ = cache.take_dirty_rows();
        let image = inserted(&mut cache, [1, 2, 3, 255]);

        let rows = cache.take_dirty_rows();
        let spans = rows.spans();
        assert_eq!(spans.len(), 1, "{spans:?}");
        let padded = cache.entries.get(image.id).expect("entry").padded;
        assert_eq!(
            spans[0],
            padded.y0 as u32..padded.y1 as u32,
            "脏行必须覆盖 padded 块（含 1 纹素 padding）"
        );
    }

    #[test]
    fn page_pixels_hold_premultiplied_bgra() {
        let mut cache = cache();
        let image = inserted(&mut cache, [128, 128, 128, 255]);
        let (dimensions, pixels) = cache.page_pixels().expect("page");
        assert_eq!(dimensions, (16, 16));

        // 内容区原点 = tile 的 UV 左边界 × 页宽（不引入内部字段就能定位）。
        let origin_x = (image.tile.u0 * 16.0).round() as usize;
        let origin_y = (image.tile.v0 * 16.0).round() as usize;
        let offset = origin_y * 16 * 4 + origin_x * 4;
        assert_eq!(&pixels[offset..offset + 4], &[128, 128, 128, 255]);
    }

    #[test]
    fn failed_insert_leaves_the_page_untouched() {
        let mut cache = cache();
        let page_before: Vec<u8> = cache.page_pixels().expect("page").1.to_vec();
        let _ = insert_error(&mut cache, b"GIF89a\x01\x00\x01\x00\x00\x00\x00");

        assert_eq!(cache.page_pixels().expect("page").1, page_before.as_slice());
        assert_eq!(cache.len(), 0);
    }
}
