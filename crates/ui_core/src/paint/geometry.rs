use std::ops::Range;

use bytes::Bytes;

use crate::{math::LogicalRect, node::id::ArenaKey, paint::atlas::AtlasKind};

pub(crate) const GEOMETRY_CHUNK_INDICES: u32 = 12288;

pub(crate) const GEOMETRY_PAGE_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GeometryId {
    index: u32,
    generation: u32,
}

impl GeometryId {
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation
    }

    #[cfg(any(test, feature = "test-support"))]
    pub const fn for_test(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }
}

impl ArenaKey for GeometryId {
    fn from_parts(index: u32, generation: u32) -> Self {
        debug_assert!(generation >= 1, "arena 的代际恒 >= 1（引擎不变量）");
        Self { index, generation }
    }

    fn parts(self) -> (u32, u32) {
        (self.index, self.generation)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexType {
    U16,
    U32,
}

impl IndexType {
    #[must_use]
    pub const fn size(self) -> u32 {
        match self {
            Self::U16 => 2,
            Self::U32 => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topology {
    TriangleList,
    TriangleStrip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryPipeline {
    Ui,
    Textured { atlas: AtlasKind },
}

impl GeometryPipeline {
    #[must_use]
    pub const fn vertex_stride(self) -> u32 {
        match self {
            Self::Ui => 12,
            Self::Textured { .. } => 16,
        }
    }

    #[must_use]
    pub(crate) const fn position_offset(self) -> u32 {
        0
    }
}

#[derive(Clone, Debug)]
pub struct GeometryDesc {
    pub vertex_bytes: Bytes,
    pub index_bytes: Bytes,
    pub vertex_stride: u32,
    pub index_type: IndexType,
    pub topology: Topology,
    pub bounds: LogicalRect,
    pub pipeline: GeometryPipeline,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeometryUpdate {
    pub revision: u64,
    pub damaged: LogicalRect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryBuffer {
    Vertex,
    Index,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GeometryChunk {
    pub index_first: u32,
    pub index_count: u32,
    pub bounds: LogicalRect,
    pub revision: u64,
}

#[derive(Debug)]
pub(crate) struct GeometrySnapshot {
    revision: u64,
    pipeline: GeometryPipeline,
    topology: Topology,
    index_type: IndexType,
    vertex_stride: u32,
    vertex_len: u32,
    index_len: u32,
    bounds: LogicalRect,
    chunks: Box<[GeometryChunk]>,
    vertex_pages: Box<[Bytes]>,
    index_pages: Box<[Bytes]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GeometryLayout {
    pub pipeline: GeometryPipeline,
    pub topology: Topology,
    pub index_type: IndexType,
    pub vertex_stride: u32,
}

impl GeometryLayout {
    pub(crate) fn of(desc: &GeometryDesc) -> Self {
        Self {
            pipeline: desc.pipeline,
            topology: desc.topology,
            index_type: desc.index_type,
            vertex_stride: desc.vertex_stride,
        }
    }
}

impl GeometrySnapshot {
    pub(crate) fn new(
        revision: u64,
        layout: GeometryLayout,
        bounds: LogicalRect,
        chunks: Box<[GeometryChunk]>,
        vertex_pages: Box<[Bytes]>,
        index_pages: Box<[Bytes]>,
    ) -> Self {
        let vertex_len = vertex_pages.iter().map(|page| page.len() as u32).sum();
        let index_len = index_pages.iter().map(|page| page.len() as u32).sum();
        Self {
            revision,
            pipeline: layout.pipeline,
            topology: layout.topology,
            index_type: layout.index_type,
            vertex_stride: layout.vertex_stride,
            vertex_len,
            index_len,
            bounds,
            chunks,
            vertex_pages,
            index_pages,
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn pipeline(&self) -> GeometryPipeline {
        self.pipeline
    }

    pub(crate) fn topology(&self) -> Topology {
        self.topology
    }

    pub(crate) fn index_type(&self) -> IndexType {
        self.index_type
    }

    pub(crate) fn vertex_stride(&self) -> u32 {
        self.vertex_stride
    }

    pub(crate) fn vertex_len(&self) -> u32 {
        self.vertex_len
    }

    pub(crate) fn index_len(&self) -> u32 {
        self.index_len
    }

    pub(crate) fn index_count(&self) -> u32 {
        self.index_len / self.index_type.size()
    }

    pub(crate) fn bounds(&self) -> LogicalRect {
        self.bounds
    }

    pub(crate) fn chunks(&self) -> &[GeometryChunk] {
        &self.chunks
    }

    pub(crate) fn chunk(&self, chunk: u32) -> Option<&GeometryChunk> {
        self.chunks.get(chunk as usize)
    }

    pub(crate) fn vertex_pages(&self) -> &[Bytes] {
        &self.vertex_pages
    }

    pub(crate) fn index_pages(&self) -> &[Bytes] {
        &self.index_pages
    }

    pub(crate) fn page_offset(page: usize) -> u32 {
        (page * GEOMETRY_PAGE_BYTES) as u32
    }
}

pub(crate) fn chunk_index_range(
    topology: Topology,
    index_count: u32,
    chunk: u32,
) -> Option<Range<u32>> {
    if index_count == 0 {
        return None;
    }
    match topology {
        Topology::TriangleStrip => (chunk == 0).then_some(0..index_count),
        Topology::TriangleList => {
            let first = chunk.checked_mul(GEOMETRY_CHUNK_INDICES)?;
            if first >= index_count {
                return None;
            }
            Some(first..(first + GEOMETRY_CHUNK_INDICES).min(index_count))
        }
    }
}

pub(crate) fn chunk_count(topology: Topology, index_count: u32) -> u32 {
    match topology {
        Topology::TriangleStrip => u32::from(index_count > 0),
        Topology::TriangleList if index_count == 0 => 0,
        Topology::TriangleList => index_count.div_ceil(GEOMETRY_CHUNK_INDICES),
    }
}

pub(crate) const FULL_INDEX_RANGE: Range<u32> = Range {
    start: 0,
    end: u32::MAX,
};

pub(crate) fn chunk_draw_range(
    topology: Topology,
    index_count: u32,
    chunk: &GeometryChunk,
    requested: Range<u32>,
) -> Option<Range<u32>> {
    let start = requested.start.min(index_count);
    let end = requested.end.min(index_count);
    if start >= end {
        return None;
    }
    let (start, end) = if topology == Topology::TriangleList {
        (start - start % 3, end - end % 3)
    } else {
        (start, end)
    };
    if start >= end {
        return None;
    }
    let first = start.max(chunk.index_first);
    let last = end.min(chunk.index_first.saturating_add(chunk.index_count));
    if first >= last {
        return None;
    }
    if topology == Topology::TriangleList {
        debug_assert!(
            first % 3 == 0 && last % 3 == 0,
            "TriangleList 的块边界必须是 3 的倍数：first={first} last={last}"
        );
    }
    Some(first..last)
}

#[cfg(test)]
mod tests {
    use ahash::HashSet;
    use bytes::Bytes;

    use super::{
        FULL_INDEX_RANGE, GEOMETRY_CHUNK_INDICES, GEOMETRY_PAGE_BYTES, GeometryChunk, GeometryDesc,
        GeometryId, GeometryPipeline, IndexType, Topology, chunk_count, chunk_draw_range,
        chunk_index_range,
    };
    use crate::{
        math::{LogicalRect, px},
        paint::atlas::AtlasKind,
    };

    #[test]
    fn index_type_sizes() {
        assert_eq!(IndexType::U16.size(), 2);
        assert_eq!(IndexType::U32.size(), 4);
    }

    #[test]
    fn pipeline_strides() {
        assert_eq!(GeometryPipeline::Ui.vertex_stride(), 12);
        assert_eq!(
            GeometryPipeline::Textured {
                atlas: AtlasKind::Image
            }
            .vertex_stride(),
            16
        );
        assert_eq!(GeometryPipeline::Ui.position_offset(), 0);
        assert_eq!(
            GeometryPipeline::Textured {
                atlas: AtlasKind::Glyph
            }
            .position_offset(),
            0
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
        let mut set = HashSet::default();
        assert!(set.insert(id));
        assert!(!set.insert(id));
        assert!(set.insert(GeometryId::for_test(1, 3)));
    }

    #[test]
    fn desc_debug_contains_fields() {
        let desc = GeometryDesc {
            vertex_bytes: Bytes::from_static(&[0, 0, 0, 0, 0, 0, 0, 0]),
            index_bytes: Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: LogicalRect::new(px(0.0), px(0.0), px(8.0), px(8.0)),
            pipeline: GeometryPipeline::Ui,
        };
        let text = format!("{desc:?}");
        assert!(text.contains("vertex_stride"));
        assert!(text.contains("pipeline"));
    }

    #[test]
    fn chunk_and_page_constants_are_stable() {
        assert_eq!(GEOMETRY_CHUNK_INDICES, 12288);
        assert_eq!(GEOMETRY_CHUNK_INDICES % 3, 0, "TriangleList 块必须整三角形");
        assert_eq!(GEOMETRY_PAGE_BYTES, 4096);
        assert_eq!(GEOMETRY_PAGE_BYTES % 512, 0, "GPU copy 对齐");
    }

    #[test]
    fn triangle_list_chunks_partition_the_index_buffer() {
        assert_eq!(chunk_count(Topology::TriangleList, 0), 0);
        assert_eq!(chunk_count(Topology::TriangleList, 12288), 1);
        assert_eq!(chunk_count(Topology::TriangleList, 12289), 2);
        assert_eq!(chunk_count(Topology::TriangleList, 200_000), 17);
        assert_eq!(
            chunk_index_range(Topology::TriangleList, 20_000, 0),
            Some(0..12288)
        );
        assert_eq!(
            chunk_index_range(Topology::TriangleList, 20_000, 1),
            Some(12288..20_000)
        );
        assert_eq!(chunk_index_range(Topology::TriangleList, 20_000, 2), None);
        let count = chunk_count(Topology::TriangleList, 20_000);
        let mut cursor = 0;
        for chunk in 0..count {
            let range = chunk_index_range(Topology::TriangleList, 20_000, chunk).expect("块存在");
            assert_eq!(range.start, cursor);
            cursor = range.end;
        }
        assert_eq!(cursor, 20_000);
    }

    #[test]
    fn triangle_strip_is_a_single_chunk() {
        assert_eq!(chunk_count(Topology::TriangleStrip, 0), 0);
        assert_eq!(chunk_count(Topology::TriangleStrip, 5), 1);
        assert_eq!(
            chunk_count(Topology::TriangleStrip, 5 * GEOMETRY_CHUNK_INDICES),
            1
        );
        assert_eq!(
            chunk_index_range(Topology::TriangleStrip, 20_000, 0),
            Some(0..20_000)
        );
        assert_eq!(chunk_index_range(Topology::TriangleStrip, 20_000, 1), None);
    }

    fn chunk(index_first: u32, index_count: u32) -> GeometryChunk {
        GeometryChunk {
            index_first,
            index_count,
            bounds: LogicalRect::default(),
            revision: 0,
        }
    }

    #[test]
    fn draw_range_inside_one_chunk_is_preserved() {
        let block = chunk(0, GEOMETRY_CHUNK_INDICES);
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &block, 30..60),
            Some(30..60)
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &block, 12_000..13_000),
            Some(12_000..12_288)
        );
    }

    #[test]
    fn draw_range_crosses_chunks_per_block() {
        let first = chunk(0, GEOMETRY_CHUNK_INDICES);
        let second = chunk(GEOMETRY_CHUNK_INDICES, 7_712);
        let range = 12_000u32..12_400;
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &first, range.clone()),
            Some(12_000..12_288)
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &second, range),
            Some(12_288..12_399)
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &second, 0..100),
            None
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &first, 12_300..12_400),
            None
        );
    }

    #[test]
    fn whole_geometry_range_covers_every_chunk() {
        let index_count = GEOMETRY_CHUNK_INDICES + 10;
        let first = chunk(0, GEOMETRY_CHUNK_INDICES);
        let tail = chunk(GEOMETRY_CHUNK_INDICES, 10);
        assert_eq!(
            chunk_draw_range(
                Topology::TriangleList,
                index_count,
                &first,
                FULL_INDEX_RANGE
            ),
            Some(0..GEOMETRY_CHUNK_INDICES)
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, index_count, &tail, FULL_INDEX_RANGE),
            Some(GEOMETRY_CHUNK_INDICES..GEOMETRY_CHUNK_INDICES + 9)
        );
        assert_eq!(FULL_INDEX_RANGE, 0..u32::MAX);
    }

    #[test]
    fn draw_range_end_clamps_to_index_count() {
        let block = chunk(0, 12);
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 12, &block, 3..u32::MAX),
            Some(3..12)
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 12, &block, 0..1_000),
            Some(0..12)
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 12, &block, 12..20),
            None
        );
    }

    #[test]
    fn empty_draw_ranges_produce_nothing() {
        let block = chunk(0, 100);
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 100, &block, 0..0),
            None
        );
        assert_eq!(
            chunk_draw_range(
                Topology::TriangleList,
                100,
                &block,
                std::ops::Range { start: 9, end: 3 }
            ),
            None,
            "start > end 是规格定义的空绘制（不是越界错误）"
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 0, &chunk(0, 0), FULL_INDEX_RANGE),
            None
        );
    }

    #[test]
    fn triangle_list_alignment_is_a_production_clamp() {
        let block = chunk(0, 100);
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 100, &block, 0..10),
            Some(0..9),
            "10 个索引只能画 3 个三角形"
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 100, &block, 1..4),
            Some(0..3),
            "未对齐的 start 向下对齐：range 起点之前最多 2 个索引一并画出（E4b 裁决）"
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 100, &block, 2..5),
            Some(0..3),
            "2..5 只覆盖网格里的第一个三角形 ⇒ 画 0..3"
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 100, &block, 1..2),
            None,
            "对齐后 start == end ⇒ 不产生 draw"
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 100, &block, 0..2),
            None,
            "不足一个三角形 ⇒ 不产生 draw"
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleStrip, 100, &block, 0..10),
            Some(0..10)
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleStrip, 100, &block, 2..7),
            Some(2..7)
        );
    }

    #[test]
    fn unaligned_start_crossing_chunks_keeps_the_global_triangle_grid() {
        let first = chunk(0, GEOMETRY_CHUNK_INDICES);
        let tail = chunk(GEOMETRY_CHUNK_INDICES, 6);
        let index_count = GEOMETRY_CHUNK_INDICES + 6;
        let requested = 12_286u32..12_294;
        assert_eq!(
            chunk_draw_range(
                Topology::TriangleList,
                index_count,
                &first,
                requested.clone()
            ),
            Some(12_285..12_288),
            "未对齐 start 向下对齐后，块 0 仍画出起点处的三角形"
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, index_count, &tail, requested),
            Some(12_288..12_294),
            "块 1 从块边界继续，不重新起算三角形"
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, index_count, &first, 12_287..12_290),
            Some(12_285..12_288)
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, index_count, &tail, 12_287..12_290),
            None,
            "range 只碰到块 0 的尾部"
        );
        for range in [
            chunk_draw_range(Topology::TriangleList, index_count, &first, 12_286..12_294),
            chunk_draw_range(Topology::TriangleList, index_count, &tail, 12_286..12_294),
        ]
        .into_iter()
        .flatten()
        {
            assert_eq!(
                (range.end - range.start) % 3,
                0,
                "每个块只画整三角形：{range:?}"
            );
        }
    }
}
