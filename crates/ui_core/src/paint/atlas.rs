//! 图集引用数据（只读）：`AtlasKind` / `AtlasTile`。
//!
//! 这两个类型是 `Primitive::GlyphRun` / `Primitive::Image` 的字段类型，属 I2 的共享数据契约，
//! 因此定义在 `paint` 而不是 `resources`（`design/DEVIATIONS.md` F-24）。图集**分配器**是 crate 私有的
//! `render/atlas.rs`（P05 落地），公开面只有这里的数据。
//!
//! 坐标与代际约定见 `design/phase-02-paint-foundation.md` §10.1–§10.3、§10.6。

/// 图集分两类（字形与图片分图集）。`#[repr(u8)]` 的值即 `GpuPrimitive.params1.y` 的 `texture_index`。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AtlasKind {
    /// 字形覆盖度：`R8`（线性、非 sRGB）。
    Glyph = 0,
    /// 图片：`BGRA8`，sRGB 编码（硬件解码）+ 直通 alpha。
    Image = 1,
}

impl AtlasKind {
    /// 该图集页在三平台纹理数组 / bindless 表里的索引（= `GpuPrimitive.params1.y`）。
    #[must_use]
    pub const fn index(self) -> u32 {
        self as u32
    }
}

/// 图集里的一块矩形 + 代际。只读数据：应用层（P15）持有它来画图标，引擎（P07）把它编码进 `uv`。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasTile {
    pub atlas: AtlasKind,
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

    /// `self.generation != atlas_generation`：P06 的缓存校验与「图集重排 → PAINT」的判据。
    #[must_use]
    pub const fn is_stale(self, atlas_generation: u32) -> bool {
        self.generation != atlas_generation
    }
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use super::{AtlasKind, AtlasTile};

    fn tile(atlas: AtlasKind, u0: f32, v0: f32, u1: f32, v1: f32, generation: u32) -> AtlasTile {
        AtlasTile {
            atlas,
            u0,
            v0,
            u1,
            v1,
            generation,
        }
    }

    #[test]
    fn atlas_kind_index_matches_texture_slots() {
        assert_eq!(AtlasKind::Glyph.index(), 0);
        assert_eq!(AtlasKind::Image.index(), 1);
    }

    #[test]
    fn tile_uv_order() {
        let t = tile(AtlasKind::Glyph, 0.25, 0.5, 0.75, 1.0, 3);
        assert_eq!(t.uv(), [0.25, 0.5, 0.75, 1.0]);
        assert!(t.u0 < t.u1);
        assert!(t.v0 < t.v1);
    }

    #[test]
    fn tile_staleness() {
        let t = tile(AtlasKind::Image, 0.0, 0.0, 0.5, 0.5, 7);
        assert!(!t.is_stale(7));
        assert!(t.is_stale(8));
    }

    #[test]
    fn size_budget() {
        // AtlasKind(1) + 4×f32 + u32，对齐 4 → 24 B。
        assert_eq!(size_of::<AtlasKind>(), 1);
        assert_eq!(size_of::<AtlasTile>(), 24);
    }
}
