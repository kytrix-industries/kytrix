//! 静态几何通道的数据契约（注册期描述 + 句柄 + 管线枚举）。
//!
//! `GeometryId` 是 `Copy` 句柄（要每帧塞进 `Primitive::Geometry`/`Path`），因此释放必须显式
//! （`App::release_geometry`），不能依赖 `Drop`（`design/DEVIATIONS.md` F-30）。
//! 顶点布局与 uniform 规格见 `design/phase-02-paint-foundation.md` §9。

use bytes::Bytes;

use crate::{math::Rect, paint::atlas::AtlasKind};

/// 已注册静态几何的句柄（带代际，防悬垂）。
///
/// 字段**私有**：句柄只能由 `GeometryStore`（P09）铸出。读取走 [`GeometryId::index`] /
/// [`GeometryId::generation`]；P02 的测试用 `for_test` 构造（`#[cfg(test)]`，不进公开面）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GeometryId {
    index: u32,
    generation: u32,
}

impl GeometryId {
    /// 该几何在 store 槽位里的下标。
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// 该几何的代际（释放后复用槽位会 +1，旧句柄因此失效）。
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation
    }

    /// 仅测试用：P09 之前没有 `GeometryStore`，而 `Primitive::Path` / `GeometryDraw` 是公开类型，
    /// 测试（含 `--features test-support` 的集成测试）必须能构造句柄。
    ///
    /// **P09 落地 `GeometryStore` 后必须删除**（store 是唯一铸造点，F-65）。
    #[cfg(any(test, feature = "test-support"))]
    pub const fn for_test(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }
}

/// 索引格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexType {
    U16,
    U32,
}

impl IndexType {
    /// 每个索引的字节数（2 / 4）。是布局契约的唯一来源（P09 校验与 P10 shader 注释共用）。
    #[must_use]
    pub const fn size(self) -> u32 {
        match self {
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }
}

/// 图元拓扑（只支持三角）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topology {
    TriangleList,
    TriangleStrip,
}

/// v1 的内置管线。顶点布局由变体唯一决定（见 [`GeometryPipeline::vertex_stride`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryPipeline {
    /// 实色三角：`[f32; 2]` 位置（逻辑像素，8 B/顶点）。`Primitive::Path` 固定用这个。
    Ui,
    /// 纹理三角：`[f32; 2]` 位置 + `[f32; 2]` 归一化 UV（16 B/顶点），采样 `atlas` 指定的图集页。
    Textured { atlas: AtlasKind },
}

impl GeometryPipeline {
    /// 该 pipeline 要求的每顶点字节数（`Ui` = 8，`Textured` = 16）。
    #[must_use]
    pub const fn vertex_stride(self) -> u32 {
        match self {
            Self::Ui => 8,
            Self::Textured { .. } => 16,
        }
    }
}

/// 静态几何的注册描述：装载期一次写入，之后不可变。
#[derive(Debug)]
pub struct GeometryDesc {
    /// 顶点字节。长度必须是 `vertex_stride` 的整数倍。
    pub vertex_bytes: Bytes,
    /// 索引字节。长度必须是 `index_type.size()` 的整数倍。
    pub index_bytes: Bytes,
    /// 每顶点字节数，必须等于 `pipeline.vertex_stride()`。
    pub vertex_stride: u32,
    pub index_type: IndexType,
    pub topology: Topology,
    /// **逻辑像素** AABB（局部空间，未经变换）。damage / 命中 / 裁剪判断的唯一来源。
    pub bounds: Rect,
    pub pipeline: GeometryPipeline,
}

#[cfg(test)]
mod tests {
    use ahash::HashSet;
    use bytes::Bytes;

    use super::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology};
    use crate::{math::Rect, paint::atlas::AtlasKind};

    #[test]
    fn index_type_sizes() {
        assert_eq!(IndexType::U16.size(), 2);
        assert_eq!(IndexType::U32.size(), 4);
    }

    #[test]
    fn pipeline_strides() {
        assert_eq!(GeometryPipeline::Ui.vertex_stride(), 8);
        assert_eq!(
            GeometryPipeline::Textured {
                atlas: AtlasKind::Image
            }
            .vertex_stride(),
            16
        );
    }

    #[test]
    fn textured_pipeline_carries_atlas() {
        let pipeline = GeometryPipeline::Textured {
            atlas: AtlasKind::Glyph,
        };
        assert_eq!(pipeline.vertex_stride(), 16);
        match pipeline {
            GeometryPipeline::Textured { atlas } => assert_eq!(atlas, AtlasKind::Glyph),
            GeometryPipeline::Ui => panic!("expected Textured"),
        }
    }

    #[test]
    fn geometry_id_is_hashable_and_reads_back() {
        let id = GeometryId::for_test(1, 2);
        assert_eq!(id.index(), 1);
        assert_eq!(id.generation(), 2);
        // 用 workspace 指定的哈希容器（P09 的 retire 集合同款）。
        let mut set = HashSet::default();
        assert!(set.insert(id));
        assert!(!set.insert(id));
        // 代际不同即不同句柄（旧句柄失效的依据）。\n        assert!(set.insert(GeometryId::for_test(1, 3)));
    }

    #[test]
    fn desc_debug_contains_fields() {
        let desc = GeometryDesc {
            vertex_bytes: Bytes::from_static(&[0, 0, 0, 0, 0, 0, 0, 0]),
            index_bytes: Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: 8,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
            pipeline: GeometryPipeline::Ui,
        };
        let text = format!("{desc:?}");
        assert!(text.contains("vertex_stride"));
        assert!(text.contains("pipeline"));
    }
}
