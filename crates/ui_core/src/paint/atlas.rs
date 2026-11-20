//! 图集引用数据（只读）：`AtlasKind` / `AtlasTile`。
//!
//! 这两个类型是 `Primitive::GlyphRun` / `Primitive::Image` 的字段类型，属 I2 的共享数据契约，
//! 因此定义在 `paint` 而不是 `resources`。图集**分配器**是 crate 私有的
//! `render/atlas.rs`（P05 落地），公开面只有这里的数据。
//!
//! 坐标与代际约定见下方常量与注释。

/// 图片图集的最大页数（D-9：多页图集）。
///
/// 单页 2048² 只能容纳约 3628 个 32×32 tile；行情列表「每个交易对一个图标」会很快打满。
/// 图片图集因此按页增长（每页 `MAX_ATLAS_PAGE²`），页数上限为 4（约 14.5k 个 32² tile）。
/// 语义：
/// - 页按需懒分配（没有图片就没有页；页缓冲/GPU 纹理都随页出现）；
/// - 页满且页数未达上限 ⇒ 开新页；达到上限且全部页满 ⇒ `DecodeCause::Capacity { PageFull }`；
/// - 已空的页按 LRU 回收（**只**回收没有活 tile 的页；tile 恒有效契约不因淘汰破坏）。
///
/// 该常量只是**上限**（页号与每窗口待上传槽的数组界），**不是** texture array 的层数：
/// 三端图片图集的层数由 App 侧 [`crate::render::ImageAtlasPager::page_count`] 决定并按需
/// 增长（见 `render::atlas::AtlasArraySpec`）。`shader` 用 `params1.y`（图集页索引）作为
/// 层号采样，不读这个上限。
pub const MAX_IMAGE_PAGES: u16 = 4;

/// 图集分两类（字形与图片分图集）。`#[repr(u8)]` 的值是**后端内部的槽位下标**（每端按
/// `AtlasKind::index()` 索引自己的图集槽）；它**不是** `GpuPrimitive.params1.z` 的 `kind`
/// （那由 `paint/instance.rs` 的 `KIND_*` 决定：Glyph = 1、Image = 2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AtlasKind {
    /// 字形覆盖度：`R8`（线性、非 sRGB）。
    Glyph = 0,
    /// 图片：`BGRA8`，sRGB 编码（硬件解码）+ 直通 alpha。
    Image = 1,
}

impl AtlasKind {
    /// 后端槽位下标（`0 = Glyph`、`1 = Image`）。**不是** shader 的 `kind` 取值。
    #[must_use]
    pub const fn index(self) -> u32 {
        self as u32
    }
}

/// 图集里的一块矩形 + 代际。只读数据：应用层（P15）持有它来画图标，引擎（P07）把它编码进
/// `uv` 与 `params1.y`（图集页索引）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasTile {
    pub atlas: AtlasKind,
    /// 图集页索引（页内 UV 的归一化基准是**这一页**的尺寸）。
    ///
    /// - `Glyph` 图集当前单页，恒 0；
    /// - `Image` 图集 `0..page_count`，由 `resources` 的多页分配器给出；编码进
    ///   `GpuPrimitive.params1.y`，shader 用它作 texture array 的层号。
    ///
    /// texture array 的层数不是常量：图片图集的层数 = App 当前打开的图集页数
    /// （`render::atlas::AtlasArraySpec`），页数增长时三端按 CPU 影子页 / App 重传重建数组，
    /// **只增不减**（缩容会让已有 tile 的 `page` 指向不存在的层）。
    pub page: u16,
    /// 归一化 UV 的左边界（**内容区**，不含 padding）。
    pub u0: f32,
    /// 归一化 UV 的上边界。
    pub v0: f32,
    /// 归一化 UV 的右边界。
    pub u1: f32,
    /// 归一化 UV 的下边界。
    pub v1: f32,
    /// 分配时的图集代际；与当前代际不等即失效。
    pub generation: u32,
}

impl AtlasTile {
    /// 直接进 `GpuPrimitive.uv`。
    #[must_use]
    pub const fn uv(self) -> [f32; 4] {
        [self.u0, self.v0, self.u1, self.v1]
    }

    /// `GpuPrimitive.params1.y` 的图集页索引（整数编码进 f32，见 contract §1）。
    #[must_use]
    pub const fn page_f32(self) -> f32 {
        self.page as f32
    }

    /// `self.generation != atlas_generation`：P06 的缓存校验与「图集重排 → PAINT」的判据。
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
        }
    }

    #[test]
    fn atlas_kind_index_matches_the_backend_slots() {
        assert_eq!(AtlasKind::Glyph.index(), 0);
        assert_eq!(AtlasKind::Image.index(), 1);
    }

    /// texture array 的层数是**运行时值**，不是 `AtlasKind` 的方法：`MAX_IMAGE_PAGES` 只是
    /// 页号 / 待上传槽的上限。图片图集的层数由 App 的分页器页数决定（P2-B3 修复：旧实现让
    /// 每窗口一旦有图就常驻 4 层 × 2048² BGRA8 = 64 MiB）。
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

    /// D-9：页索引编码进 `params1.y`（整数编码进 f32，contract §1 的 `[0, 2^24)` 约束）。
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
        // AtlasKind(1) + page(u16, 2) 后接对齐 4 的 4×f32 + u32：仍为 24 B。
        assert_eq!(size_of::<AtlasKind>(), 1);
        assert_eq!(size_of::<AtlasTile>(), 24);
    }
}
