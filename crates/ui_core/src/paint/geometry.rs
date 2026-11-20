//! 静态几何通道的数据契约（注册描述 / 句柄 / 管线枚举 / 跨线程快照 / range 更新回执）。
//!
//! # 两层数据模型（X5）
//!
//! 应用只认识 [`GeometryDesc`]（注册输入）与 [`GeometryId`]（句柄）。引擎内部把几何数据切成
//! **固定大小的块**（[`GEOMETRY_CHUNK_INDICES`] 个索引/块，TriangleList 下 = 4096 个三角形），
//! 并维护**页级**的快照（[`GEOMETRY_PAGE_BYTES`] / 页）：
//!
//! - **块**（`GeometryChunk`）是绘制/失效单位：paint 的一条块 lane、编码器的一条几何 op、
//!   damage 的一个矩形都对应一个块；块之间在索引缓冲里连续，因此绘制序与单块完全一致。
//! - **页**（[`GeometrySnapshot::vertex_pages`] / [`GeometrySnapshot::index_pages`]）是 GPU
//!   增量上传单位：一次 [`GeometryStore::update_vertices`] / `update_indices` 只重建被触碰的
//!   4 KiB 页，三端后端按页比较（`ptr_eq`）后只上传变化的页。
//!
//! 块大小**不是公开 API**：应用看不到 chunk，只能通过 range 更新表达「改了哪段数据」；绘制侧的
//! 应用可见区间是 `GeometryDraw.range`（索引元素，§11.4 / E4），引擎用同一个块表与
//! [`chunk_draw_range`] 把它裁剪成逐块的 `DrawIndexed(first_index, index_count)`。
//!
//! # 为什么用不可变快照
//!
//! 几何数据跨线程消费（UI 线程写 `GeometryStore`，合成线程的 `GeometryCatalog` 与 GPU 线程
//! 只读快照）。如果就地共享同一份 `Vec<u8>`，更新会与在途帧产生数据竞争。快照把「更新」表达为
//! 新版本：未变的页共享同一个 `Bytes`（引用计数），只有被写的页复制一次；GPU 侧按页
//! `ptr_eq` 找到差异，因此一次更新 = 一次 4 KiB 级复制 + 一次页级上传。
//!
//! 与成熟实现的对应：
//! - Skia：几何（`GrDirectContext` + `SkVertices`）注册后整份上传，顶点复用靠 display list；
//! - Qt `QSGGeometry`：`markDirty(DirtyGeometry)` 只重传脏顶点区间；
//! - 本引擎的「页」相当于把 Qt 的脏区间量化到 4 KiB 网格，使跨线程快照可以不可变。

use std::ops::Range;

use bytes::Bytes;

use crate::{math::Rect, node::id::ArenaKey, paint::atlas::AtlasKind};

/// 每个几何块的**最大索引数**（`TriangleList` 下 = 4096 个三角形；`12288 = 4096 × 3`）。
///
/// 取值依据：
/// - 一次追加跨块时只需重建/重传**一个**尾块（K 线场景的主诉求）；
/// - 单块 = 一个绘制 op + 一条 lane：块太小会让一个 20 万点折线产生上百条 op/lane（draw call
///   与绘制序都膨胀），块太大则热力图改一格要重传的字节数上升。4096 三角形 ≈ 48 KiB 索引 +
///   144 KiB 顶点（Ui 12 B/顶点），是「一次局部更新 ≤ 数十 KiB」与「op 数 ≤ 两位」的折中。
///
/// `TriangleStrip` 无法在不复制边界顶点的前提下切块，因此**退化为单块**（文档化 + 测试）。
/// 消费者：`GeometryStore` 的分块与 `paint/pass.rs` 的块 lane 规划。
pub(crate) const GEOMETRY_CHUNK_INDICES: u32 = 12288;

/// GPU 增量上传的页大小（字节）。
///
/// 页是 `GeometrySnapshot` 里 `Bytes` 的最小粒度：一次更新只复制/上传被触碰的页。4 KiB 与
/// 典型 GPU copy 对齐要求（`D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT` = 512 B 的整数倍）兼容，
/// 同时让「热力图改一格颜色」这类小更新落在 1 页（≤ 4 KiB 上传）而不是整块 48 KiB。
pub(crate) const GEOMETRY_PAGE_BYTES: usize = 4096;

/// 已注册静态几何的句柄（带代际，防悬垂）。
///
/// 字段**私有**：句柄只能由 `GeometryStore` 铸出。读取走 [`GeometryId::index`] /
/// [`GeometryId::generation`]；测试用 `for_test` 构造（`#[cfg(any(test, feature = "test-support"))]`）。
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

    /// 仅测试用：`Primitive::Path` / `GeometryDraw` 是公开类型，测试必须能构造句柄。
    ///
    /// 生产代码的铸造点只有 `GeometryStore::insert`。
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

/// 索引格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexType {
    U16,
    U32,
}

impl IndexType {
    /// 每个索引的字节数（2 / 4）。是布局契约的唯一来源（注册校验与三端索引格式共用）。
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
    /// 实色三角 + **解析抗锯齿**：`[f32; 2]` 位置（逻辑像素）+ `f32` 覆盖率距离（逻辑像素，
    /// 负 = 形状内部），12 B/顶点。`Primitive::Path` 固定用这个。
    ///
    /// **覆盖率网格契约（AR2 P2-③）**：顶点只携带**一个标量**有符号距离，片元用
    /// `aa_coverage`（带宽 = `length(ddx, ddy)`）换算成覆盖率。因此要让 AA 带宽恒为 1 px 且
    /// 不出现「尖角削角 / 共享边双重混合」，应用必须保证：
    ///
    /// 1. 距离场沿法向线性（`|∇d| = 1`），AA 带内不得插值非线性距离；
    /// 2. AA 带三角形**互不重叠**（相邻三角形共享边时覆盖率只由一侧写入，否则
    ///    `0.5 ⊕ 0.5 = 0.75` 会出现暗线）；
    /// 3. 尖角按 Skia `GrAATriangulator` 的做法为每条边生成独立外扩三角形（每条边的顶点带
    ///    自己的覆盖率），而不是复用同一个顶点标量。
    ///
    /// 引擎**不做** tessellation，也不检测上述条件；不满足时只表现为走样，不会报错。
    /// 这与 Skia 的差别在分工：Skia 在 CPU 侧完成三角化 + 覆盖率顶点生成，本引擎把它交给应用。
    Ui,
    /// 纹理三角：`[f32; 2]` 位置 + `[f32; 2]` 归一化 UV（16 B/顶点），采样 `atlas` 指定的图集页。
    Textured { atlas: AtlasKind },
}

impl GeometryPipeline {
    /// 该 pipeline 要求的每顶点字节数（`Ui` = 12，`Textured` = 16）。
    #[must_use]
    pub const fn vertex_stride(self) -> u32 {
        match self {
            Self::Ui => 12,
            Self::Textured { .. } => 16,
        }
    }

    /// 顶点字节里位置属性的偏移（字节）。所有 v1 管线的位置都是第一个 `[f32; 2]`，块包围盒的
    /// 计算（`GeometryStore` 的内存 AABB 扫描）依赖这条契约。
    #[must_use]
    pub(crate) const fn position_offset(self) -> u32 {
        0
    }
}

/// 静态几何的注册描述：装载期一次写入，之后通过 range 更新修改（不再整体重注册）。
///
/// Clone 是廉价的：Bytes 是引用计数切片，克隆只加引用（顶点字节不复制）。
#[derive(Clone, Debug)]
pub struct GeometryDesc {
    /// 顶点字节。长度必须是 `vertex_stride` 的整数倍。
    pub vertex_bytes: Bytes,
    /// 索引字节。长度必须是 `index_type.size()` 的整数倍。
    pub index_bytes: Bytes,
    /// 每顶点字节数，必须等于 `pipeline.vertex_stride()`。
    pub vertex_stride: u32,
    pub index_type: IndexType,
    pub topology: Topology,
    /// **逻辑像素** AABB（几何局部空间 = 逻辑窗口坐标，未经 draw.basis 变换）。
    ///
    /// 用途：整块几何绘制（`Primitive::Path` / `Canvas::push_geometry`）的 damage 范围，以及
    /// `App::set_geometry_transform` 的旧/新可见范围。**命中测试仍用节点布局盒** —— 几何不参与
    /// 命中形状计算，交互区域由节点盒决定（应用需要更大的热区时给节点设 size）。
    ///
    /// 块级绘制（几何节点 polyline/heatmap 等）的 damage 用引擎按顶点位置现算的**块 AABB**，
    /// 不受这个字段影响；本字段只需覆盖整块几何即可（应用给不出精确值时可以给保守值）。
    pub bounds: Rect,
    pub pipeline: GeometryPipeline,
}

/// 一次 `GeometryStore::update_vertices` / `update_indices` / `truncate` 成功的回执。
///
/// `damaged` 是**受影响块**「旧 bounds ∪ 新 bounds」的并集（逻辑像素），与 `update_*` 同口径：
/// 顶点位移时旧位置与新位置都必须重画；截断时 = 被移除块的**旧 AABB** ∪ 变短尾块的
/// **旧 ∪ 新 AABB**（收缩时新通常是旧的子集，代码仍按两侧并集计算，保持与 update 一致）。
/// 没有可见变化（比如追加了一段暂时未被索引引用的顶点、截断到当前长度）时为空矩形。
///
/// 消费者：`App::update_geometry_*` / `App::truncate_geometry` 把它并入绑定节点的标脏判断
/// （节点由引擎自动标脏，应用不需要自己再声明 damage）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeometryUpdate {
    /// 更新后的几何版本。
    ///
    /// - `update_vertices` / `update_indices`：**只要真的改了数据就前进**（即使没有块受影响、
    ///   没有可见变化也会前进；GPU 侧据此判断快照新旧）；
    /// - `truncate`：真的缩短时前进；**两个目标都等于当前长度（合法 no-op）时返回当前版本、
    ///   不前进**（没有可上传的差异，也不 damage / 不标脏）。
    pub revision: u64,
    /// 受影响块的旧 ∪ 新 AABB（逻辑像素；空 = 本次更新没有改变任何块的可见范围）。
    pub damaged: Rect,
}

/// range 更新作用的缓冲类别（[`crate::error::EngineError`] 的几何更新错误按它分类）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryBuffer {
    Vertex,
    Index,
}

/// 一个**块**的只读元数据（跨线程快照的一部分）。
///
/// 块在索引缓冲里连续：块 `k` 覆盖索引 `[k × GEOMETRY_CHUNK_INDICES, min((k+1) × …, index_count))`。
/// `TriangleStrip` 恒只有一块（`index_first = 0`）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GeometryChunk {
    /// 块内第一个索引（索引单位，不是字节）。
    pub index_first: u32,
    /// 块内索引数（尾块可以不满）。绘制时 `first_index = index_first`。
    pub index_count: u32,
    /// 块内图元的 AABB（逻辑像素；由引擎扫描该块索引引用的顶点位置得出，未含 AA 外扩 ——
    /// damage 在编码期统一外扩 1 px）。
    pub bounds: Rect,
    /// 块的数据版本（任何触碰该块块数据的 range 更新都会推进；paint 的块 lane 复用它）。
    pub revision: u64,
}

/// 跨线程的几何数据**不可变快照**（`Arc` 共享；页级 COW）。
///
/// 生产者：`GeometryStore`（UI 线程，每次注册 / range 更新后重建）。消费者：合成线程的
/// `GeometryCatalog`、编码器（块元数据）、GPU 线程（页差异 → 增量上传）。
///
/// 快照里的 `vertex_pages` / `index_pages` 是**页对齐**的连续切分：第 i 页覆盖字节区间
/// `[i × GEOMETRY_PAGE_BYTES, min((i+1) × GEOMETRY_PAGE_BYTES, len))`。页 `Bytes` 在未变的
/// 更新之间**共享同一份分配**，因此 GPU 侧可以逐页 `ptr_eq` 找差异。
#[derive(Debug)]
pub(crate) struct GeometrySnapshot {
    /// 几何版本（单调；与 `GeometryUpdate::revision` 同源）。
    revision: u64,
    pipeline: GeometryPipeline,
    topology: Topology,
    index_type: IndexType,
    vertex_stride: u32,
    vertex_len: u32,
    index_len: u32,
    /// 注册期声明的整体 AABB（整块几何绘制 / 变换 damage 用；块绘制用 `chunks[].bounds`）。
    bounds: Rect,
    chunks: Box<[GeometryChunk]>,
    vertex_pages: Box<[Bytes]>,
    index_pages: Box<[Bytes]>,
}

/// 几何布局（注册期固定、range 更新不可变的那部分字段）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GeometryLayout {
    pub pipeline: GeometryPipeline,
    pub topology: Topology,
    pub index_type: IndexType,
    pub vertex_stride: u32,
}

impl GeometryLayout {
    /// 从注册描述提取并校验前的布局（校验在 `GeometryStore::insert`）。
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
    /// 构造快照（唯一生产者：`GeometryStore`；`missing` 快照除外）。
    ///
    /// 两个长度由页表推导（页是连续的页对齐切分），不单独传参 —— 少一个可以不一致的真相。
    pub(crate) fn new(
        revision: u64,
        layout: GeometryLayout,
        bounds: Rect,
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

    /// 几何版本。
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

    /// 顶点字节长度。
    pub(crate) fn vertex_len(&self) -> u32 {
        self.vertex_len
    }

    /// 索引字节长度。
    pub(crate) fn index_len(&self) -> u32 {
        self.index_len
    }

    /// 索引个数（`index_len / index_type.size()`；GPU 的 `DrawIndexed` 上界）。
    pub(crate) fn index_count(&self) -> u32 {
        self.index_len / self.index_type.size()
    }

    /// 注册期声明的整体 AABB（逻辑像素）。
    pub(crate) fn bounds(&self) -> Rect {
        self.bounds
    }

    /// 全部块元数据（按绘制序）。
    pub(crate) fn chunks(&self) -> &[GeometryChunk] {
        &self.chunks
    }

    /// 第 `chunk` 块的元数据。
    pub(crate) fn chunk(&self, chunk: u32) -> Option<&GeometryChunk> {
        self.chunks.get(chunk as usize)
    }

    pub(crate) fn vertex_pages(&self) -> &[Bytes] {
        &self.vertex_pages
    }

    pub(crate) fn index_pages(&self) -> &[Bytes] {
        &self.index_pages
    }

    /// 第 `page` 页在对应缓冲里的字节偏移。
    pub(crate) fn page_offset(page: usize) -> u32 {
        (page * GEOMETRY_PAGE_BYTES) as u32
    }
}

/// 计算某个块在**给定索引长度**下的索引区间（与 `GeometryStore` 的分块策略同一实现）。
///
/// 消费者：`GeometryStore` 建块、paint/编码器校验快照块数、三端后端的 draw range。
/// `TriangleStrip` 恒一块；`TriangleList` 按 [`GEOMETRY_CHUNK_INDICES`] 切。
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

/// 索引长度对应的块数（`TriangleList` = `ceil(index_count / GEOMETRY_CHUNK_INDICES)`；
/// `TriangleStrip` = 1（非空）/ 0（空））。
pub(crate) fn chunk_count(topology: Topology, index_count: u32) -> u32 {
    match topology {
        Topology::TriangleStrip => u32::from(index_count > 0),
        Topology::TriangleList if index_count == 0 => 0,
        Topology::TriangleList => index_count.div_ceil(GEOMETRY_CHUNK_INDICES),
    }
}

/// [`crate::paint::primitive::GeometryDraw::range`] 的「整份几何」哨兵：`0..u32::MAX`。
///
/// 用 `u32::MAX` 而不是 `0..index_count`：几何长度可被 `update_*` / `truncate_geometry` 改变，
/// 哨兵让「整份」在几何增长后仍然成立；真正的上界在绘制期按 `index_count` 与块表裁剪（§11.4）。
pub(crate) const FULL_INDEX_RANGE: Range<u32> = Range {
    start: 0,
    end: u32::MAX,
};

/// 把一次绘制的**索引元素**区间裁剪到一个块的绘制区间（§11.4 的 range × 块求交纯函数）。
///
/// 消费者：`paint/pass.rs`（几何节点的块 lane 取本块的绘制区间）与 `paint/instance.rs`
/// （整块几何绘制 / Canvas 内绘制按 range 展开成逐块 op）。三端 GPU encoder 只消费这里算出的
/// `first_index/count`，因此不存在第二处 range 语义。
///
/// 语义：
/// - `requested` 单位是**索引元素**，与 [`IndexType`] 无关；`TriangleStrip` 同样按索引计；
/// - 超出 `index_count` 的一端 **clamp**（几何可能被 `truncate_geometry` 变短，不报错）；
/// - `start >= end` / `start >= index_count` / 与块不相交 ⇒ `None`（空绘制：无 op、无 damage）；
/// - `TriangleList` 的绘制长度截到 3 的倍数：GPU 的三角形列表只消费整三角形，不足一个三角形的
///   尾部索引本来就画不出像素。`TriangleStrip` 不截。
///
/// 返回值是**索引缓冲里的绝对区间**（不是块内相对偏移）：
/// `DrawIndexed(first_index = range.start, index_count = range.end - range.start)`。
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
    let first = start.max(chunk.index_first);
    let last = end.min(chunk.index_first.saturating_add(chunk.index_count));
    if first >= last {
        return None;
    }
    let mut count = last - first;
    if topology == Topology::TriangleList {
        count -= count % 3;
    }
    (count > 0).then_some(first..first + count)
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
    use crate::{math::Rect, paint::atlas::AtlasKind};

    #[test]
    fn index_type_sizes() {
        assert_eq!(IndexType::U16.size(), 2);
        assert_eq!(IndexType::U32.size(), 4);
    }

    #[test]
    fn pipeline_strides() {
        // R05-6：Ui 顶点 = position(f32×2) + 解析 AA 的覆盖率距离(f32)，共 12 B。
        assert_eq!(GeometryPipeline::Ui.vertex_stride(), 12);
        assert_eq!(
            GeometryPipeline::Textured {
                atlas: AtlasKind::Image
            }
            .vertex_stride(),
            16
        );
        // 块包围盒扫描依赖「位置在顶点字节偏移 0」这条契约。
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
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
            pipeline: GeometryPipeline::Ui,
        };
        let text = format!("{desc:?}");
        assert!(text.contains("vertex_stride"));
        assert!(text.contains("pipeline"));
    }

    /// 块大小常量是公开文档的一部分：4096 三角形（12288 索引）/ 4 KiB 页。
    #[test]
    fn chunk_and_page_constants_are_stable() {
        assert_eq!(GEOMETRY_CHUNK_INDICES, 12288);
        assert_eq!(GEOMETRY_CHUNK_INDICES % 3, 0, "TriangleList 块必须整三角形");
        assert_eq!(GEOMETRY_PAGE_BYTES, 4096);
        assert_eq!(GEOMETRY_PAGE_BYTES % 512, 0, "GPU copy 对齐");
    }

    #[test]
    fn triangle_list_chunks_partition_the_index_buffer() {
        // 0 / 恰一块 / 一块多一个 / 多块。
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
        // 块区间必须首尾相接、覆盖整个索引缓冲。
        let count = chunk_count(Topology::TriangleList, 20_000);
        let mut cursor = 0;
        for chunk in 0..count {
            let range = chunk_index_range(Topology::TriangleList, 20_000, chunk).expect("块存在");
            assert_eq!(range.start, cursor);
            cursor = range.end;
        }
        assert_eq!(cursor, 20_000);
    }

    /// TriangleStrip 不能跨块：边界三角形需要共享顶点，切块会破坏图元 ⇒ 单块退化。
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

    // ── E4：range × 块求交（`chunk_draw_range`）──────────────────────────────

    fn chunk(index_first: u32, index_count: u32) -> GeometryChunk {
        GeometryChunk {
            index_first,
            index_count,
            bounds: Rect::default(),
            revision: 0,
        }
    }

    /// 单块内的子区间原样保留（range 单位 = 索引元素，与 `IndexType` 无关）。
    #[test]
    fn draw_range_inside_one_chunk_is_preserved() {
        let block = chunk(0, GEOMETRY_CHUNK_INDICES);
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &block, 30..60),
            Some(30..60)
        );
        // 起点在块内、终点跨到下一块：本块只画到块尾。
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &block, 12_000..13_000),
            Some(12_000..12_288)
        );
    }

    /// 跨块：同一个 range 分别与两块求交，各得一块（块间顺序由调用方按块序遍历保证）。
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
            // 112 个索引里只有 111 个能组成整三角形。
            Some(12_288..12_399)
        );
        // 完全落在块外的 range 与该块不相交。
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &second, 0..100),
            None
        );
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 20_000, &first, 12_300..12_400),
            None
        );
    }

    /// 整份哨兵：每块拿满自己的区间；尾部不满块按实际长度（不足一个三角形截掉）。
    #[test]
    fn whole_geometry_range_covers_every_chunk() {
        // 4096 三角形 + 10 个索引（3 个完整三角形 + 1 个孤立索引）。
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

    /// 越界一端 clamp 到 `index_count`（truncate 变短后不报错），不产生越界读。
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
        // start 超出索引长度 ⇒ 空绘制（规格：`range.start >= index_count`）。
        assert_eq!(
            chunk_draw_range(Topology::TriangleList, 12, &block, 12..20),
            None
        );
    }

    /// 空区间：`start == end`、`start > end`、与块不相交都返回 `None`（无 op、无 damage）。
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

    /// `TriangleList` 的非对齐长度截到整三角形；`TriangleStrip` 保持索引语义、不截。
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
            Some(1..4)
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
}
