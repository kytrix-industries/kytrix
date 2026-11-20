//! 静态几何的注册、range 更新与生命周期：`GeometryStore`（X5）。
//!
//! # 为什么需要它
//!
//! `GeometryId` 的字段是私有的，唯一合法的铸造点是 [`GeometryStore`]。应用只在装载期注册
//! （[`GeometryStore::insert`]），之后用 **range 更新**（`update_vertices` / `update_indices`）
//! 原地修改顶点/索引子区间，或用 **成对截断**（[`GeometryStore::truncate`]）缩短滑动窗口，
//! 而不是每 tick 重注册整段几何。
//!
//! # 与成熟实现的关系
//!
//! 语义对齐 Qt 的 `QSGGeometry::markDirty(DirtyGeometry)`（局部更新顶点/索引）与 Skia 的
//! display item 复用（几何数据不被重新收集），区别在于**跨线程**：本引擎的合成线程/GPU 线程
//! 只读不可变快照，因此 store 把更新表达为「新版本快照」：
//!
//! ```text
//! 应用线程                           GeometryStore（CPU）                  合成/GPU 线程
//! ─────────                          ───────────────────                   ────────────
//! insert(desc) ─────────────────►    块表 + 4 KiB 页 + 快照 v1
//! update_vertices(range, bytes) ─►   原地改数据 + 只复制被触碰的页 → v2
//! Primitive::Geometry(id) ───────►   每帧只传 Copy 句柄 + 块元数据
//!                                                       └─► GPU 按页 ptr_eq 找差异，只上传变化页
//! release(id) ───────────────────►   槽位释放（代际 +1，旧句柄立即失效）
//! ```
//!
//! # 分块与页
//!
//! - **块**（[`crate::paint::geometry::GEOMETRY_CHUNK_INDICES`] 索引/块）：绘制与失效单位。
//!   索引缓冲按块连续切分，因此块间绘制序 = 原始索引序；`TriangleStrip` 退化为单块。
//! - **页**（[`crate::paint::geometry::GEOMETRY_PAGE_BYTES`]）：跨线程快照的复制/上传单位。
//!
//! 块包围盒由引擎按「该块索引引用的顶点位置」现算（所有 v1 管线的位置都在顶点字节偏移 0），
//! 因此 damage 不需要应用声明；零高度 / 零宽度的线状几何把尺寸抬到 ≥ 1 逻辑像素，保证
//! damage 非空（否则水平折线改一段会没有脏矩形）。
//!
//! # 代际校验
//!
//! 槽位复用后代际 +1，旧句柄永久失效。渲染层查表时必须**同时**比对 index 与 generation，
//! 否则释放后再注册会让旧图元画成新几何 —— 这是静默的视觉错误，不是崩溃。

use std::{
    ops::Range,
    sync::{Arc, OnceLock},
};

use bytes::Bytes;
use smol_str::{SmolStr, format_smolstr};

use crate::{
    error::EngineError,
    math::{Point, Rect},
    node::id::Arena,
    paint::geometry::{
        GEOMETRY_PAGE_BYTES, GeometryBuffer, GeometryChunk, GeometryDesc, GeometryId,
        GeometryLayout, GeometrySnapshot, GeometryUpdate, chunk_count, chunk_index_range,
    },
};

/// 块包围盒的最小逻辑尺寸（见模块文档：水平/垂直折线也不能产生空 damage）。
const MIN_CHUNK_EXTENT: f32 = 1.0;

/// 一个已注册几何的 CPU 侧记录（**可变**：range 更新在这里原地写）。
#[derive(Debug)]
struct GeometryRecord {
    /// 注册期固定：管线 / 拓扑 / 索引格式 / 顶点步长。
    layout: GeometryLayout,
    /// 顶点字节（原地修改；`update_vertices` 的唯一写点）。
    vertex: Vec<u8>,
    /// 索引字节（原地修改；`update_indices` 的唯一写点）。
    index: Vec<u8>,
    /// 注册期声明的整体 AABB（逻辑像素；整块几何绘制 / 变换 damage 用）。
    bounds: Rect,
    /// 几何版本（单调递增；每次真正改到数据 +1）。
    revision: u64,
    /// 块元数据（按索引序；块数随索引长度变化：追加增长 / 截断缩短）。
    chunks: Vec<ChunkRecord>,
    /// 顶点页（`bytes::Bytes` 不可变；未变的页跨更新共享同一份分配）。
    vertex_pages: Vec<Bytes>,
    /// 索引页（同上）。
    index_pages: Vec<Bytes>,
    /// 与 `revision` 一致的不可变快照（更新后立即重建；读者只拿 `Arc` 克隆）。
    snapshot: Arc<GeometrySnapshot>,
}

/// 一块的 CPU 侧元数据（`GeometryChunk` 的 store 内扩：多出顶点区间，用于把顶点 range
/// 更新精确映射到块）。
#[derive(Debug)]
struct ChunkRecord {
    index_first: u32,
    index_count: u32,
    /// 该块索引引用到的顶点下标区间 `[first, end)`（含端点：`first` 是 min，`end` 是 max+1；
    /// 没有合法索引时 `end <= first`，表示空区间）。
    vertex_first: u32,
    vertex_end: u32,
    bounds: Rect,
    revision: u64,
}

/// 块元数据扫描结果。
#[derive(Clone, Copy, Debug)]
struct ChunkScan {
    vertex_first: u32,
    vertex_end: u32,
    bounds: Rect,
}

/// 静态几何的进程级存储（`App` 持有一份；与窗口无关）。
///
/// 几何数据是**设备无关**的 CPU 字节：同一个 `GeometryId` 在所有窗口/所有 GPU 后端上都有效，
/// 各后端自行缓存自己的顶点/索引缓冲（单设备多窗口时天然共享）。
///
/// **线程约束**：单写者（UI 线程）。`update_*` 与 `insert`/`release` 一样只在 UI 线程调用；
/// 快照（[`GeometryStore::snapshot_arc`]）才是跨线程只读面。
pub struct GeometryStore {
    entries: Arena<GeometryRecord, GeometryId>,
    /// 版本计数器（块与几何共用；单调递增，不随 store 清空重置）。
    revision: u64,
    /// 已注册数量的历史峰值（诊断：几何泄漏会让它单调上升）。
    peak: usize,
}

impl GeometryStore {
    /// 空 store（消费者：`App::new`）。
    pub(crate) fn new() -> Self {
        Self {
            entries: Arena::new(),
            revision: 0,
            peak: 0,
        }
    }

    /// 下一版本号。
    fn next_revision(&mut self) -> u64 {
        self.revision += 1;
        self.revision
    }

    /// 注册一份静态几何，返回句柄。
    ///
    /// 校验（全部是**应用错误**，返回 `Err` 而不是 panic）：
    /// - `vertex_stride` 必须等于 `pipeline.vertex_stride()`；
    /// - `vertex_bytes.len()` 必须是 `vertex_stride` 的整数倍且非空；
    /// - `index_bytes.len()` 必须是 `index_type.size()` 的整数倍且非空；
    /// - `bounds` 必须是有限且非负尺寸（整块绘制的 damage 只用它）。
    ///
    /// # Errors
    /// [`EngineError::GeometryRegister`]：上述任一校验失败。
    pub fn insert(&mut self, desc: GeometryDesc) -> Result<GeometryId, EngineError> {
        let expected_stride = desc.pipeline.vertex_stride();
        if desc.vertex_stride != expected_stride {
            return Err(register_error(format_smolstr!(
                "vertex_stride {} does not match pipeline stride {expected_stride}",
                desc.vertex_stride
            )));
        }
        let stride = desc.vertex_stride as usize;
        if desc.vertex_bytes.is_empty() || !desc.vertex_bytes.len().is_multiple_of(stride) {
            return Err(register_error(format_smolstr!(
                "vertex_bytes len {} is not a positive multiple of stride {stride}",
                desc.vertex_bytes.len()
            )));
        }
        let index_size = desc.index_type.size() as usize;
        if desc.index_bytes.is_empty() || !desc.index_bytes.len().is_multiple_of(index_size) {
            return Err(register_error(format_smolstr!(
                "index_bytes len {} is not a positive multiple of index size {index_size}",
                desc.index_bytes.len()
            )));
        }
        if !desc.bounds.is_finite() || desc.bounds.size.width < 0.0 || desc.bounds.size.height < 0.0
        {
            return Err(register_error(SmolStr::new_static(
                "geometry bounds must be finite with non-negative size",
            )));
        }

        let layout = GeometryLayout::of(&desc);
        let vertex = desc.vertex_bytes.to_vec();
        let index = desc.index_bytes.to_vec();
        let revision = self.next_revision();
        let index_count = (index.len() / index_size) as u32;
        let mut chunks = Vec::with_capacity(chunk_count(layout.topology, index_count) as usize);
        for chunk in 0..chunk_count(layout.topology, index_count) {
            let range = chunk_index_range(layout.topology, index_count, chunk)
                .expect("chunk < chunk_count 时区间必须存在");
            let count = range.end - range.start;
            let scan = scan_chunk(&layout, &vertex, &index, range.start, count);
            chunks.push(ChunkRecord {
                index_first: range.start,
                index_count: count,
                vertex_first: scan.vertex_first,
                vertex_end: scan.vertex_end,
                bounds: scan.bounds,
                revision,
            });
        }
        let vertex_pages = build_pages(&vertex);
        let index_pages = build_pages(&index);
        let snapshot = Arc::new(GeometrySnapshot::new(
            revision,
            layout,
            desc.bounds,
            chunk_metadata(&chunks),
            vertex_pages.clone().into_boxed_slice(),
            index_pages.clone().into_boxed_slice(),
        ));
        let id = self.entries.insert_with(|_| GeometryRecord {
            layout,
            vertex,
            index,
            bounds: desc.bounds,
            revision,
            chunks,
            vertex_pages,
            index_pages,
            snapshot,
        });
        self.peak = self.peak.max(self.entries.len());
        Ok(id)
    }

    /// 释放一份几何（代际 +1，旧句柄立即失效）。
    ///
    /// 返回 `false` = 句柄陈旧（已释放过或从未存在）；陈旧释放是**幂等无害**的。
    pub fn release(&mut self, id: GeometryId) -> bool {
        self.entries.remove(id).is_some()
    }

    /// 按句柄取当前不可变快照（**同时**校验 index 与 generation）。
    ///
    /// 返回 `Arc` 克隆，不复制字节：页 `Bytes` 与未变的更新共享同一份分配。
    pub(crate) fn snapshot_arc(&self, id: GeometryId) -> Option<Arc<GeometrySnapshot>> {
        self.entries
            .get(id)
            .map(|record| Arc::clone(&record.snapshot))
    }

    /// range 更新**顶点**字节。
    ///
    /// `range` 是字节区间（半开），`bytes` 长度必须与 `range.len()` 相等；`range.start` 与
    /// `range.len()` 必须按 `vertex_stride` 对齐。允许在缓冲区**末尾追加**：
    /// `range.end` 可以超过当前长度（此时缓冲区增长到 `range.end`），但 `range.start` 不能
    /// 超过当前长度（不允许在中间留空洞）。
    ///
    /// 合法的空区间（`start == end`）是 **no-op**：不退款、不推进版本、不产生 damage。
    ///
    /// 受影响块 = 顶点区间与该块索引引用到的顶点区间相交的块；对每个受影响块重算 AABB，
    /// 返回值里的 `damaged` 是「旧 bounds ∪ 新 bounds」的并集（顶点位移时旧位置也要重画）。
    ///
    /// # Errors
    /// - [`EngineError::UnknownGeometry`]：句柄陈旧 / 已释放；
    /// - [`EngineError::GeometryRangeBytes`]：`bytes.len()` 与区间长度不一致；
    /// - [`EngineError::GeometryRangeUnaligned`]：区间未按顶点步长对齐；
    /// - [`EngineError::GeometryRangeOutOfBounds`]：区间越界（含 `start > len`）。
    pub fn update_vertices(
        &mut self,
        id: GeometryId,
        range: Range<u32>,
        bytes: &[u8],
    ) -> Result<GeometryUpdate, EngineError> {
        let Some(record) = self.entries.get_mut(id) else {
            return Err(EngineError::UnknownGeometry { geometry: id });
        };
        validate_range(
            id,
            GeometryBuffer::Vertex,
            &range,
            record.vertex.len(),
            record.layout.vertex_stride,
            bytes.len(),
        )?;
        if range.is_empty() {
            return Ok(GeometryUpdate {
                revision: record.revision,
                damaged: Rect::default(),
            });
        }
        let stride = record.layout.vertex_stride as usize;
        let vertex_range = range.start as usize / stride..range.end as usize / stride;
        // 1) 原地写数据（末尾追加时先增长）。
        if range.end as usize > record.vertex.len() {
            record.vertex.resize(range.end as usize, 0);
        }
        record.vertex[range.start as usize..range.end as usize].copy_from_slice(bytes);
        // 2) 只重建被触碰的页。
        rebuild_pages(
            &record.vertex,
            &mut record.vertex_pages,
            range.start as usize..range.end as usize,
        );
        // 3) 受影响的块：块的顶点引用区间与更新区间相交（保守：用 min/max 区间）。
        let mut affected = Vec::new();
        for (chunk, meta) in record.chunks.iter().enumerate() {
            if meta.vertex_end > meta.vertex_first
                && meta.vertex_first < vertex_range.end as u32
                && (vertex_range.start as u32) < meta.vertex_end
            {
                affected.push(chunk);
            }
        }
        // 版本在同一片 store 里单调：直接操作字段（`self.entries` 的借用不覆盖 `self.revision`）。
        self.revision += 1;
        let revision = self.revision;
        let mut damaged = Rect::default();
        for chunk in affected {
            let (index_first, index_count) = {
                let meta = &record.chunks[chunk];
                (meta.index_first, meta.index_count)
            };
            let scan = scan_chunk(
                &record.layout,
                &record.vertex,
                &record.index,
                index_first,
                index_count,
            );
            let meta = &mut record.chunks[chunk];
            damaged = damaged.union(meta.bounds).union(scan.bounds);
            meta.vertex_first = scan.vertex_first;
            meta.vertex_end = scan.vertex_end;
            meta.bounds = scan.bounds;
            meta.revision = revision;
        }
        record.revision = revision;
        record.snapshot = rebuild_snapshot(record);
        Ok(GeometryUpdate { revision, damaged })
    }

    /// range 更新**索引**字节。
    ///
    /// 契约与 [`GeometryStore::update_vertices`] 对称：`range` 是字节区间、`bytes` 长度相等、
    /// 按 `index_type.size()` 对齐；允许末尾追加（增长会创建新的尾块）。
    ///
    /// 受影响块 = 索引区间与该块索引区间相交的块（追加时还包括新建的块）；每块重算 AABB 与
    /// 顶点引用区间，`damaged` 是旧 ∪ 新的并集。
    ///
    /// # Errors
    /// 同 [`GeometryStore::update_vertices`]（`buffer` 字段为 `Index`）。
    pub fn update_indices(
        &mut self,
        id: GeometryId,
        range: Range<u32>,
        bytes: &[u8],
    ) -> Result<GeometryUpdate, EngineError> {
        let Some(record) = self.entries.get_mut(id) else {
            return Err(EngineError::UnknownGeometry { geometry: id });
        };
        let index_size = record.layout.index_type.size();
        validate_range(
            id,
            GeometryBuffer::Index,
            &range,
            record.index.len(),
            index_size,
            bytes.len(),
        )?;
        if range.is_empty() {
            return Ok(GeometryUpdate {
                revision: record.revision,
                damaged: Rect::default(),
            });
        }
        // 1) 原地写数据（末尾追加时先增长）。
        if range.end as usize > record.index.len() {
            record.index.resize(range.end as usize, 0);
        }
        record.index[range.start as usize..range.end as usize].copy_from_slice(bytes);
        rebuild_pages(
            &record.index,
            &mut record.index_pages,
            range.start as usize..range.end as usize,
        );
        // 2) 块表：块数只随索引增长而增加；新块按块策略补建。
        let index_count = (record.index.len() / index_size as usize) as u32;
        let wanted = chunk_count(record.layout.topology, index_count);
        while record.chunks.len() < wanted as usize {
            record.chunks.push(ChunkRecord {
                index_first: 0,
                index_count: 0,
                vertex_first: 0,
                vertex_end: 0,
                bounds: Rect::default(),
                revision: 0,
            });
        }
        // 3) 受影响块：与**增长后**的块索引区间相交的块 + 本次新建的块。
        let index_range =
            range.start as usize / index_size as usize..range.end as usize / index_size as usize;
        let mut affected: Vec<usize> = Vec::new();
        for (chunk, meta) in record.chunks.iter().enumerate() {
            let Some(chunk_range) =
                chunk_index_range(record.layout.topology, index_count, chunk as u32)
            else {
                continue;
            };
            let intersects = (chunk_range.start as usize) < index_range.end
                && index_range.start < chunk_range.end as usize;
            let fresh = meta.revision == 0;
            if intersects || fresh {
                affected.push(chunk);
            }
        }
        self.revision += 1;
        let revision = self.revision;
        let mut damaged = Rect::default();
        for chunk in affected {
            let (index_first, count) = {
                let total = (record.index.len() / index_size as usize) as u32;
                match chunk_index_range(record.layout.topology, total, chunk as u32) {
                    Some(range) => (range.start, range.end - range.start),
                    None => (0, 0),
                }
            };
            let scan = scan_chunk(
                &record.layout,
                &record.vertex,
                &record.index,
                index_first,
                count,
            );
            let meta = &mut record.chunks[chunk];
            damaged = damaged.union(meta.bounds).union(scan.bounds);
            meta.index_first = index_first;
            meta.index_count = count;
            meta.vertex_first = scan.vertex_first;
            meta.vertex_end = scan.vertex_end;
            meta.bounds = scan.bounds;
            meta.revision = revision;
        }
        record.revision = revision;
        record.snapshot = rebuild_snapshot(record);
        Ok(GeometryUpdate { revision, damaged })
    }

    /// **成对截断**顶点 / 索引缓冲（X5 滑动窗口：删掉最旧的 N 根 K 线 + 追加新的）。
    ///
    /// 参数顺序固定为 `(vertices, indices)`：`vertices` 是目标**顶点个数**，`indices` 是目标
    /// **索引个数**（都是元素个数，不是字节数），各自必须 `<=` 当前个数。两个字面量相邻且同型，
    /// 调用处容易传反 —— 建议用命名局部变量 / 注释写清单位，例如 K 线（每根 4 顶点 + 6 索引、
    /// 保留 `roots` 根）：
    ///
    /// ```text
    /// let vertices = roots * 4;
    /// let indices = roots * 6;
    /// store.truncate(id, vertices, indices)?;
    /// ```
    ///
    /// 截断只能缩短，增长走 [`GeometryStore::update_vertices`] /
    /// [`GeometryStore::update_indices`]（尾随追加）。两个目标都等于当前个数时是 **no-op**：不推进
    /// 版本（返回当前 `revision`）、不产生 damage、不重建快照。
    ///
    /// 语义：
    /// - 数据先截断，再按新长度重写页表：末端页按实际长度存、多余页丢弃；未触碰的页保持原
    ///   `Bytes`（GPU 侧仍按页 `ptr_eq` 做增量上传）。
    /// - 块数按 `chunk_count(topology, indices)` 重算（只减）；`damaged` = 被移除块的**旧 AABB**
    ///   ∪ 变短尾块的**旧 ∪ 新 AABB**（与 `update_*` 的「旧 ∪ 新」同口径；收缩时新通常是旧的
    ///   子集）。
    /// - 尾块（最后一个保留块）的 block revision 只在它的索引区间 / 顶点引用 / AABB 真的变化时
    ///   推进；paint 因此只重建这条块 lane，其余块 lane 整段复用。截断到 0 块时节点仍保留一条
    ///   承载装饰（背景/边框/焦点环/节点盒 damage）的 lane，几何 op 不发射（见 `paint/pass.rs`）。
    /// - 截断后可以继续用 `update_vertices` / `update_indices` 做**尾随追加**（同一条追加路径）。
    ///
    /// 契约：应用必须保证截断后剩余索引不引用被移除的顶点（与 `insert` 一样，引擎只校验结构，
    /// 不校验索引值的合法性；越界索引在 GPU 侧是未定义行为）。
    ///
    /// # Errors
    /// - [`EngineError::UnknownGeometry`]：句柄陈旧 / 已释放；
    /// - [`EngineError::GeometryTruncateGrows`]：目标个数大于当前个数（`requested` / `current`
    ///   指向具体缓冲，应用据此修参数或改走增长 API）。
    pub fn truncate(
        &mut self,
        id: GeometryId,
        vertices: u32,
        indices: u32,
    ) -> Result<GeometryUpdate, EngineError> {
        let Some(record) = self.entries.get_mut(id) else {
            return Err(EngineError::UnknownGeometry { geometry: id });
        };
        let stride = record.layout.vertex_stride as usize;
        let index_size = record.layout.index_type.size() as usize;
        let current_vertices = (record.vertex.len() / stride) as u32;
        let current_indices = (record.index.len() / index_size) as u32;
        for (buffer, requested, current) in [
            (GeometryBuffer::Vertex, vertices, current_vertices),
            (GeometryBuffer::Index, indices, current_indices),
        ] {
            if requested > current {
                return Err(EngineError::GeometryTruncateGrows {
                    geometry: id,
                    buffer,
                    requested,
                    current,
                });
            }
        }
        if vertices == current_vertices && indices == current_indices {
            return Ok(GeometryUpdate {
                revision: record.revision,
                damaged: Rect::default(),
            });
        }
        // 1) 数据 + 页：先截断数据，再缩短页表（末端页按实际长度重写、多余页丢弃）。
        record.vertex.truncate(vertices as usize * stride);
        truncate_pages(&record.vertex, &mut record.vertex_pages);
        record.index.truncate(indices as usize * index_size);
        truncate_pages(&record.index, &mut record.index_pages);
        // 2) 块表：块数只减。先按**旧**块表算 damage（被移除块的旧 AABB）。
        let wanted = chunk_count(record.layout.topology, indices);
        let previous_chunks = record.chunks.len();
        let mut damaged = Rect::default();
        for chunk in wanted as usize..previous_chunks {
            damaged = damaged.union(record.chunks[chunk].bounds);
        }
        self.revision += 1;
        let revision = self.revision;
        // 3) 尾块：索引区间变短（或顶点被截断）时重扫 AABB / 顶点引用；只有真的变化才推进
        //    该块 revision（paint 据此决定是否重建这条块 lane）。
        if let Some(tail) = wanted.checked_sub(1) {
            let tail = tail as usize;
            let old = &record.chunks[tail];
            let old_bounds = old.bounds;
            let old_count = old.index_count;
            let old_vertex_first = old.vertex_first;
            let old_vertex_end = old.vertex_end;
            let range = chunk_index_range(record.layout.topology, indices, tail as u32)
                .expect("tail < chunk_count 时区间必须存在");
            let count = range.end - range.start;
            if count != old_count || vertices < current_vertices {
                let scan = scan_chunk(
                    &record.layout,
                    &record.vertex,
                    &record.index,
                    range.start,
                    count,
                );
                let changed = count != old_count
                    || scan.vertex_first != old_vertex_first
                    || scan.vertex_end != old_vertex_end
                    || scan.bounds != old_bounds;
                if changed {
                    damaged = damaged.union(old_bounds).union(scan.bounds);
                    let meta = &mut record.chunks[tail];
                    meta.index_first = range.start;
                    meta.index_count = count;
                    meta.vertex_first = scan.vertex_first;
                    meta.vertex_end = scan.vertex_end;
                    meta.bounds = scan.bounds;
                    meta.revision = revision;
                }
            }
        }
        record.chunks.truncate(wanted as usize);
        record.revision = revision;
        record.snapshot = rebuild_snapshot(record);
        Ok(GeometryUpdate { revision, damaged })
    }

    /// 已注册数量（诊断 / 测试）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.len() == 0
    }

    /// 历史峰值（诊断：几何泄漏会让它单调上升）。
    #[must_use]
    pub fn peak(&self) -> usize {
        self.peak
    }
}

/// 构造 [`EngineError::GeometryRegister`]（注册校验失败共用）。
fn register_error(detail: SmolStr) -> EngineError {
    EngineError::GeometryRegister { detail }
}

/// range 更新的静态校验（越界 / 对齐 / 字节数）。
fn validate_range(
    id: GeometryId,
    buffer: GeometryBuffer,
    range: &Range<u32>,
    buffer_len: usize,
    align: u32,
    bytes_len: usize,
) -> Result<(), EngineError> {
    // 起点不能越过当前末尾（允许末尾追加，但不允许在中间留空洞）；反向区间是非法构造。
    if range.start as usize > buffer_len || range.end < range.start {
        return Err(EngineError::GeometryRangeOutOfBounds {
            geometry: id,
            buffer,
            range: range.clone(),
            len: buffer_len as u32,
        });
    }
    let byte_len = range.end - range.start;
    if !range.start.is_multiple_of(align) || !byte_len.is_multiple_of(align) {
        return Err(EngineError::GeometryRangeUnaligned {
            geometry: id,
            buffer,
            range: range.clone(),
            align,
        });
    }
    if byte_len as usize != bytes_len {
        return Err(EngineError::GeometryRangeBytes {
            geometry: id,
            buffer,
            range: range.clone(),
            bytes: bytes_len,
        });
    }
    Ok(())
}

/// 扫描一块：顶点引用区间（min..max+1）与该块图元的 AABB（逻辑像素）。
fn scan_chunk(
    layout: &GeometryLayout,
    vertex: &[u8],
    index: &[u8],
    index_first: u32,
    index_count: u32,
) -> ChunkScan {
    let stride = layout.vertex_stride as usize;
    let vertex_count = (vertex.len() / stride) as u32;
    let index_size = layout.index_type.size() as usize;
    let position_offset = layout.pipeline.position_offset() as usize;
    let mut first = u32::MAX;
    let mut end = 0u32;
    let mut min = Point::new(f32::INFINITY, f32::INFINITY);
    let mut max = Point::new(f32::NEG_INFINITY, f32::NEG_INFINITY);
    let mut has_position = false;
    for step in 0..index_count {
        let position = (index_first + step) as usize * index_size;
        let Some(raw) = index.get(position..position + index_size) else {
            break;
        };
        let vertex_index = match layout.index_type {
            crate::paint::geometry::IndexType::U16 => {
                u32::from(u16::from_le_bytes([raw[0], raw[1]]))
            }
            crate::paint::geometry::IndexType::U32 => {
                u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])
            }
        };
        if vertex_index >= vertex_count {
            // 越界索引是应用错误（GPU 侧另有未定义行为），扫描时跳过而不是 panic：
            // 不把它算进 AABB，避免把一个错误坐标放大成整帧 damage。
            continue;
        }
        first = first.min(vertex_index);
        end = end.max(vertex_index + 1);
        let offset = vertex_index as usize * stride + position_offset;
        let Some(pos) = vertex.get(offset..offset + 8) else {
            continue;
        };
        let x = f32::from_le_bytes([pos[0], pos[1], pos[2], pos[3]]);
        let y = f32::from_le_bytes([pos[4], pos[5], pos[6], pos[7]]);
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        min = Point::new(min.x.min(x), min.y.min(y));
        max = Point::new(max.x.max(x), max.y.max(y));
        has_position = true;
    }
    if first == u32::MAX {
        first = 0;
        end = 0;
    }
    let bounds = if has_position {
        // 零宽/零高的线状几何也必须有非空 damage：尺寸抬到 ≥ `MIN_CHUNK_EXTENT`。
        Rect::new(
            min.x,
            min.y,
            (max.x - min.x).max(MIN_CHUNK_EXTENT),
            (max.y - min.y).max(MIN_CHUNK_EXTENT),
        )
    } else {
        Rect::default()
    };
    ChunkScan {
        vertex_first: first,
        vertex_end: end,
        bounds,
    }
}

/// 页表：按 [`GEOMETRY_PAGE_BYTES`] 切分数据。
fn build_pages(data: &[u8]) -> Vec<Bytes> {
    let mut pages = Vec::with_capacity(data.len().div_ceil(GEOMETRY_PAGE_BYTES));
    for chunk in data.chunks(GEOMETRY_PAGE_BYTES) {
        pages.push(Bytes::copy_from_slice(chunk));
    }
    pages
}

/// 重建 `range` 覆盖的页（未碰的页保持原 `Bytes`：GPU 侧按 `ptr_eq` 判等）。
fn rebuild_pages(data: &[u8], pages: &mut Vec<Bytes>, range: Range<usize>) {
    if range.is_empty() {
        return;
    }
    let first = range.start / GEOMETRY_PAGE_BYTES;
    let last = (range.end - 1) / GEOMETRY_PAGE_BYTES;
    while pages.len() <= last {
        pages.push(Bytes::new());
    }
    for (page, slot) in pages.iter_mut().enumerate().take(last + 1).skip(first) {
        let start = page * GEOMETRY_PAGE_BYTES;
        let end = (start + GEOMETRY_PAGE_BYTES).min(data.len());
        *slot = Bytes::copy_from_slice(&data[start..end]);
    }
}

/// 缩短页表以匹配**已截断**的数据：末端页按实际长度重写、多余页丢弃。
///
/// 未触碰的整页保持原 `Bytes`（引用计数共享），因此截断后的 GPU 上传仍只覆盖新末页。
fn truncate_pages(data: &[u8], pages: &mut Vec<Bytes>) {
    let keep = data.len().div_ceil(GEOMETRY_PAGE_BYTES);
    pages.truncate(keep);
    if !data.len().is_multiple_of(GEOMETRY_PAGE_BYTES)
        && let Some(page) = pages.last_mut()
    {
        let start = (keep - 1) * GEOMETRY_PAGE_BYTES;
        *page = Bytes::copy_from_slice(&data[start..]);
    }
}

/// 把 store 内块的元数据转成跨线程快照的块表。
fn chunk_metadata(chunks: &[ChunkRecord]) -> Box<[GeometryChunk]> {
    chunks
        .iter()
        .map(|meta| GeometryChunk {
            index_first: meta.index_first,
            index_count: meta.index_count,
            bounds: meta.bounds,
            revision: meta.revision,
        })
        .collect()
}

/// 用当前记录重建不可变快照。
fn rebuild_snapshot(record: &GeometryRecord) -> Arc<GeometrySnapshot> {
    Arc::new(GeometrySnapshot::new(
        record.revision,
        record.layout,
        record.bounds,
        chunk_metadata(&record.chunks),
        record.vertex_pages.clone().into_boxed_slice(),
        record.index_pages.clone().into_boxed_slice(),
    ))
}

/// 一个**空**几何快照（未注册句柄的兜底；GPU 侧报告 `GeometryRegister`）。
///
/// `paint` 对未注册句柄仍会发一条几何 op（旧行为：GPU 侧报错而不是静默不画），编码器需要
/// 一个「有一块、零长度」的快照来构造这条 op。
pub(crate) fn missing_snapshot() -> Arc<GeometrySnapshot> {
    static MISSING: OnceLock<Arc<GeometrySnapshot>> = OnceLock::new();
    Arc::clone(MISSING.get_or_init(|| {
        Arc::new(GeometrySnapshot::new(
            0,
            GeometryLayout {
                pipeline: crate::paint::geometry::GeometryPipeline::Ui,
                topology: crate::paint::geometry::Topology::TriangleList,
                index_type: crate::paint::geometry::IndexType::U16,
                vertex_stride: 12,
            },
            Rect::default(),
            Box::new([GeometryChunk {
                index_first: 0,
                index_count: 0,
                bounds: Rect::default(),
                revision: 0,
            }]),
            Box::new([]),
            Box::new([]),
        ))
    }))
}

/// 仅测试：把一个注册描述变成合法快照（Windows WARP 单测 / 合成器测试用）。
///
/// 生产代码没有这条路径：快照只由本 store 的记录产出。macOS / Linux 的后端测试直接走
/// `GeometryStore`，不需要这条旁路（`cfg(all(test, windows))` 避免其它平台的 dead_code）。
#[cfg(all(test, windows))]
pub(crate) fn snapshot_for_test(desc: GeometryDesc) -> Arc<GeometrySnapshot> {
    let mut store = GeometryStore::new();
    let id = store
        .insert(desc)
        .expect("test geometry desc must be valid");
    store.snapshot_arc(id).expect("just inserted")
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::GeometryStore;
    use crate::{
        error::EngineError,
        math::Rect,
        paint::{
            atlas::AtlasKind,
            geometry::{
                GEOMETRY_CHUNK_INDICES, GEOMETRY_PAGE_BYTES, GeometryBuffer, GeometryDesc,
                GeometryPipeline, IndexType, Topology,
            },
        },
    };

    /// 一个合法的 12 B/顶点 + u16 索引描述（单位三角形：3 顶点 × 12 B = 36 B）。
    fn triangle_desc() -> GeometryDesc {
        let mut vertex_bytes = [0u8; 36];
        let points: [[f32; 3]; 3] = [[0.0, 0.0, -1.0], [1.0, 0.0, -1.0], [0.0, 2.0, -1.0]];
        for (slot, point) in vertex_bytes.as_chunks_mut::<12>().0.iter_mut().zip(points) {
            slot[..4].copy_from_slice(&point[0].to_le_bytes());
            slot[4..8].copy_from_slice(&point[1].to_le_bytes());
            slot[8..].copy_from_slice(&point[2].to_le_bytes());
        }
        GeometryDesc {
            vertex_bytes: Bytes::copy_from_slice(&vertex_bytes),
            index_bytes: Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 1.0, 2.0),
            pipeline: GeometryPipeline::Ui,
        }
    }

    /// 顶点坐标编码（Ui 布局：position + coverage）。
    fn vertex(x: f32, y: f32) -> [u8; 12] {
        let mut bytes = [0u8; 12];
        bytes[..4].copy_from_slice(&x.to_le_bytes());
        bytes[4..8].copy_from_slice(&y.to_le_bytes());
        bytes[8..].copy_from_slice(&(-1.0f32).to_le_bytes());
        bytes
    }

    fn vertex_bytes(points: &[[f32; 2]]) -> Vec<u8> {
        points
            .iter()
            .flat_map(|point| vertex(point[0], point[1]))
            .collect()
    }

    fn index_bytes_u16(indices: &[u16]) -> Vec<u8> {
        indices
            .iter()
            .flat_map(|index| index.to_le_bytes())
            .collect()
    }

    /// 大几何：`count` 个三角形，顶点互不共享（3 顶点/三角形，正好每块 4096 三角形）。
    fn long_desc(triangles: u32) -> GeometryDesc {
        let mut points = Vec::with_capacity(triangles as usize * 3);
        let mut indices = Vec::with_capacity(triangles as usize * 3);
        for triangle in 0..triangles {
            let base = (triangle * 3) as u16;
            let x = (triangle / 100) as f32;
            let y = (triangle % 100) as f32;
            points.push([x, y]);
            points.push([x + 1.0, y]);
            points.push([x, y + 1.0]);
            indices.extend_from_slice(&[base, base + 1, base + 2]);
        }
        GeometryDesc {
            vertex_bytes: Bytes::from(vertex_bytes(&points)),
            index_bytes: Bytes::from(index_bytes_u16(&indices)),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, triangles as f32, 100.0),
            pipeline: GeometryPipeline::Ui,
        }
    }

    fn snapshot(
        store: &GeometryStore,
        id: crate::paint::geometry::GeometryId,
    ) -> std::sync::Arc<crate::paint::geometry::GeometrySnapshot> {
        store.snapshot_arc(id).expect("registered")
    }

    #[test]
    fn insert_returns_a_usable_handle() {
        let mut store = GeometryStore::new();
        assert!(store.is_empty());
        let id = store.insert(triangle_desc()).expect("valid desc");
        assert_eq!(store.len(), 1);
        let snapshot = snapshot(&store, id);
        assert_eq!(snapshot.vertex_len(), 36);
        assert_eq!(snapshot.index_len(), 6);
        assert_eq!(snapshot.index_count(), 3);
        assert_eq!(snapshot.bounds(), Rect::new(0.0, 0.0, 1.0, 2.0));
        assert_eq!(snapshot.chunks().len(), 1);
        let chunk = snapshot.chunk(0).expect("chunk");
        assert_eq!((chunk.index_first, chunk.index_count), (0, 3));
        // 块 AABB 由顶点现算：(0,0)-(1,2)。
        assert_eq!(chunk.bounds, Rect::new(0.0, 0.0, 1.0, 2.0));
    }

    #[test]
    fn handles_are_dense_and_distinct() {
        let mut store = GeometryStore::new();
        let first = store.insert(triangle_desc()).expect("first");
        let second = store.insert(triangle_desc()).expect("second");
        assert_ne!(first, second);
        assert_eq!(store.len(), 2);
        assert_eq!(store.peak(), 2);
    }

    #[test]
    fn release_invalidates_the_handle() {
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("valid desc");
        assert!(store.release(id));
        assert!(store.snapshot_arc(id).is_none(), "释放后必须查不到");
        assert!(store.is_empty());
        assert!(!store.release(id), "重复释放是幂等的 false");
    }

    #[test]
    fn a_reused_slot_does_not_resurrect_the_old_handle() {
        let mut store = GeometryStore::new();
        let old = store.insert(triangle_desc()).expect("old");
        assert!(store.release(old));
        let new = store.insert(triangle_desc()).expect("new");
        assert_eq!(new.index(), old.index(), "槽位必须复用");
        assert_ne!(new.generation(), old.generation(), "代际必须递增");
        assert!(store.snapshot_arc(old).is_none(), "旧句柄必须失效");
        assert!(store.snapshot_arc(new).is_some());
    }

    #[test]
    fn registration_validation_is_unchanged() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.vertex_stride = 16;
        assert!(store.insert(desc).is_err());
        assert!(store.is_empty(), "失败的注册不得留下槽位");

        let mut desc = triangle_desc();
        desc.vertex_bytes = Bytes::from_static(&[0, 0, 0]);
        assert!(store.insert(desc).is_err());

        let mut desc = triangle_desc();
        desc.vertex_bytes = Bytes::new();
        assert!(store.insert(desc).is_err());
        let mut desc = triangle_desc();
        desc.index_bytes = Bytes::new();
        assert!(store.insert(desc).is_err());

        let mut desc = triangle_desc();
        desc.index_bytes = Bytes::from_static(&[0, 0, 1]);
        assert!(store.insert(desc).is_err());

        for bounds in [
            Rect::new(f32::NAN, 0.0, 1.0, 1.0),
            Rect::new(0.0, 0.0, -1.0, 1.0),
            Rect::new(0.0, 0.0, 1.0, f32::INFINITY),
        ] {
            let mut desc = triangle_desc();
            desc.bounds = bounds;
            assert!(store.insert(desc).is_err(), "bounds {bounds:?} 必须被拒绝");
        }
    }

    #[test]
    fn ui_pipeline_rejects_the_legacy_8_byte_layout() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.vertex_stride = 8;
        desc.vertex_bytes = Bytes::from_static(&[0; 24]);
        let error = store.insert(desc).expect_err("8 B 的 Ui 顶点必须被拒绝");
        assert!(error.to_string().contains("vertex_stride"));
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn textured_pipeline_uses_the_16_byte_stride() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.pipeline = GeometryPipeline::Textured {
            atlas: AtlasKind::Image,
        };
        desc.vertex_stride = 16;
        desc.vertex_bytes = Bytes::from_static(&[0; 48]);
        let id = store.insert(desc).expect("textured desc");
        assert_eq!(snapshot(&store, id).vertex_len(), 48);
    }

    #[test]
    fn u32_indices_are_counted_by_four_bytes() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.index_type = IndexType::U32;
        desc.index_bytes = Bytes::from_static(&[0; 12]);
        let id = store.insert(desc).expect("u32 desc");
        assert_eq!(snapshot(&store, id).index_len(), 12);
    }

    #[test]
    fn failed_insert_does_not_move_the_peak() {
        let mut store = GeometryStore::new();
        let _ = store.insert(triangle_desc()).expect("valid");
        let mut bad = triangle_desc();
        bad.vertex_stride = 99;
        assert!(store.insert(bad).is_err());
        assert_eq!(store.peak(), 1);
    }

    // ── range 更新：边界 / 非法输入 ─────────────────────────────────────────

    #[test]
    fn empty_range_is_a_noop() {
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("valid");
        let before = snapshot(&store, id);
        let update = store.update_vertices(id, 12..12, &[]).expect("空区间合法");
        assert_eq!(update.revision, before.revision(), "空区间不推进版本");
        assert!(update.damaged.is_empty(), "空区间不产生 damage");
        let update = store.update_indices(id, 6..6, &[]).expect("空区间合法");
        assert_eq!(update.revision, before.revision());
        assert!(update.damaged.is_empty());
        assert_eq!(snapshot(&store, id).revision(), before.revision());
    }

    #[test]
    fn unknown_handle_is_a_distinct_error() {
        let mut store = GeometryStore::new();
        let id = crate::paint::geometry::GeometryId::for_test(7, 1);
        match store.update_vertices(id, 0..12, &[0; 12]) {
            Err(EngineError::UnknownGeometry { geometry }) => assert_eq!(geometry, id),
            other => panic!("陈旧句柄必须报 UnknownGeometry，实际 {other:?}"),
        }
        assert!(!store.release(id));
    }

    #[test]
    fn out_of_bounds_and_unaligned_are_distinct_errors() {
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("valid");
        // 起点越过末尾（中间留空洞）⇒ 越界。
        match store.update_vertices(id, 48..60, &[0; 12]) {
            Err(EngineError::GeometryRangeOutOfBounds { buffer, len, .. }) => {
                assert_eq!(buffer, GeometryBuffer::Vertex);
                assert_eq!(len, 36);
            }
            other => panic!("越界必须报 GeometryRangeOutOfBounds，实际 {other:?}"),
        }
        // 6 B 不是 stride(12) 的整数倍 ⇒ 非对齐。
        match store.update_vertices(id, 0..6, &[0; 6]) {
            Err(EngineError::GeometryRangeUnaligned { buffer, align, .. }) => {
                assert_eq!(buffer, GeometryBuffer::Vertex);
                assert_eq!(align, 12);
            }
            other => panic!("非对齐必须报 GeometryRangeUnaligned，实际 {other:?}"),
        }
        // 索引按 2 B 对齐。
        match store.update_indices(id, 1..3, &[0; 2]) {
            Err(EngineError::GeometryRangeUnaligned { buffer, align, .. }) => {
                assert_eq!(buffer, GeometryBuffer::Index);
                assert_eq!(align, 2);
            }
            other => panic!("索引非对齐必须报 GeometryRangeUnaligned，实际 {other:?}"),
        }
        // 字节数与区间长度不一致。
        match store.update_vertices(id, 0..12, &[0; 6]) {
            Err(EngineError::GeometryRangeBytes { buffer, bytes, .. }) => {
                assert_eq!(buffer, GeometryBuffer::Vertex);
                assert_eq!(bytes, 6);
            }
            other => panic!("字节数不一致必须报 GeometryRangeBytes，实际 {other:?}"),
        }
        match store.update_indices(id, 0..4, &[0; 2]) {
            Err(EngineError::GeometryRangeBytes { buffer, bytes, .. }) => {
                assert_eq!(buffer, GeometryBuffer::Index);
                assert_eq!(bytes, 2);
            }
            other => panic!("字节数不一致必须报 GeometryRangeBytes，实际 {other:?}"),
        }
    }

    // ── range 更新：数据/块/页/版本 ─────────────────────────────────────────

    #[test]
    fn vertex_update_rebuilds_only_the_touched_pages() {
        // 4 页顶点（16 KiB）：更新第 1 页里的一段（起点按 12 B 对齐），只有该页的 Bytes 换新。
        let triangles = 4 * GEOMETRY_PAGE_BYTES as u32 / 36; // 每 36 B 一个三角形
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(triangles)).expect("valid");
        let before = snapshot(&store, id);
        assert!(before.vertex_pages().len() >= 4);
        let second_page = Bytes::clone(&before.vertex_pages()[1]);
        // 第 1 页的起点 4096 不是 12 的倍数：取页内第一个对齐偏移 4104（342 × 12）。
        let offset = 342 * 12;
        assert!((GEOMETRY_PAGE_BYTES..2 * GEOMETRY_PAGE_BYTES).contains(&offset));
        let update = store
            .update_vertices(id, offset as u32..(offset + 12) as u32, &vertex(9.0, 9.0))
            .expect("更新第 1 页");
        assert!(update.revision > before.revision());
        let after = snapshot(&store, id);
        assert_eq!(after.revision(), update.revision);
        assert_eq!(
            after.vertex_pages()[0].as_ptr(),
            before.vertex_pages()[0].as_ptr(),
            "未触碰的页必须共享同一份 Bytes"
        );
        assert_ne!(
            after.vertex_pages()[1].as_ptr(),
            second_page.as_ptr(),
            "被触碰的页必须换新"
        );
        // 数据真的改到了目标位置（页内偏移 = 4104 − 4096 = 8）。
        assert_eq!(&after.vertex_pages()[1][8..20], &vertex(9.0, 9.0));
    }

    #[test]
    fn vertex_update_damage_is_the_union_of_the_old_and_new_bounds() {
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("valid");
        let before = snapshot(&store, id);
        assert_eq!(
            before.chunk(0).expect("chunk").bounds,
            Rect::new(0.0, 0.0, 1.0, 2.0)
        );
        // 把**全部**顶点搬到 (100,100) 一带：旧 bounds 完全消失，damage 必须仍覆盖旧位置。
        let moved = vertex_bytes(&[[100.0, 100.0], [101.0, 100.0], [100.0, 101.0]]);
        let update = store
            .update_vertices(id, 0..36, &moved)
            .expect("移动全部顶点");
        assert!(
            update.damaged.origin.x <= 0.0
                && update.damaged.origin.y <= 0.0
                && update.damaged.max().x >= 101.0
                && update.damaged.max().y >= 101.0,
            "damage 必须是旧 ∪ 新：{:?}",
            update.damaged
        );
        let after = snapshot(&store, id);
        let bounds = after.chunk(0).expect("chunk").bounds;
        assert!(
            bounds.origin.x >= 100.0 && bounds.origin.y >= 100.0,
            "新块 bounds 只覆盖新位置：{bounds:?}"
        );
    }

    #[test]
    fn cross_chunk_vertex_update_touches_exactly_the_intersecting_chunks() {
        // 两块：第一块顶点下标 [0,12288)，第二块从 12288 开始。
        let triangles = 4096 + 10;
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(triangles)).expect("valid");
        let before = snapshot(&store, id);
        assert_eq!(before.chunks().len(), 2);
        let second_revision = before.chunk(1).expect("chunk").revision;
        // 顶点 6000（第 2000 个三角形的第二个顶点）属于第一块（顶点区间 [0, 12288)）。
        let vertex_bytes_before = before.vertex_pages().len();
        let offset = 6000 * 12;
        let update = store
            .update_vertices(id, offset as u32..(offset + 12) as u32, &vertex(77.0, 88.0))
            .expect("更新第一块里的顶点");
        let after = snapshot(&store, id);
        assert_ne!(
            after.chunk(0).expect("chunk").revision,
            before.chunk(0).expect("chunk").revision,
            "第一块必须更新"
        );
        assert_eq!(
            after.chunk(1).expect("chunk").revision,
            second_revision,
            "第二块必须不动"
        );
        assert!(
            update.damaged.max().x >= 77.0,
            "第一块的 damage 必须包含新位置：{:?}",
            update.damaged
        );
        assert_eq!(after.vertex_pages().len(), vertex_bytes_before);
    }

    #[test]
    fn append_vertices_only_extends_and_does_not_dirty_existing_chunks() {
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(4096)).expect("valid");
        let before = snapshot(&store, id);
        let chunk_revision = before.chunk(0).expect("chunk").revision;
        // 追加一个不被索引引用的顶点：数据增长，但没有块受影响。
        let append_at = before.vertex_len();
        let update = store
            .update_vertices(id, append_at..append_at + 12, &vertex(5.0, 5.0))
            .expect("追加顶点");
        assert!(update.damaged.is_empty(), "未被引用的顶点不产生 damage");
        let after = snapshot(&store, id);
        assert_eq!(after.vertex_len(), before.vertex_len() + 12);
        assert_eq!(
            after.chunk(0).expect("chunk").revision,
            chunk_revision,
            "追加顶点不得弄脏已有块"
        );
        assert_eq!(after.vertex_pages().len(), before.vertex_pages().len() + 1);
    }

    #[test]
    fn append_indices_only_rebuilds_the_tail_chunk() {
        // 一块满 + 少量尾块：追加索引只动尾块。
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(4096 + 2)).expect("valid");
        let before = snapshot(&store, id);
        assert_eq!(before.chunks().len(), 2);
        let first_revision = before.chunk(0).expect("chunk").revision;
        let tail_revision = before.chunk(1).expect("chunk").revision;
        let append_at = before.index_len();
        // 追加一个三角形（3 个 u16 索引，引用新顶点）。
        let first_vertex = (before.vertex_len() / 12) as u16;
        let update = store
            .update_vertices(
                id,
                before.vertex_len()..before.vertex_len() + 36,
                &vertex_bytes(&[[200.0, 200.0], [201.0, 200.0], [200.0, 201.0]]),
            )
            .expect("追加顶点");
        assert!(update.damaged.is_empty());
        let update = store
            .update_indices(
                id,
                append_at..append_at + 6,
                &index_bytes_u16(&[first_vertex, first_vertex + 1, first_vertex + 2]),
            )
            .expect("追加索引");
        let after = snapshot(&store, id);
        assert_eq!(after.chunks().len(), 2);
        assert_eq!(
            after.chunk(0).expect("chunk").revision,
            first_revision,
            "追加只能动尾块（第一块不得重建）"
        );
        assert_ne!(
            after.chunk(1).expect("chunk").revision,
            tail_revision,
            "尾块必须重建"
        );
        assert!(
            update.damaged.max().x >= 201.0,
            "尾块的新 bounds 必须并进 damage：{:?}",
            update.damaged
        );
        assert_eq!(after.chunk(1).expect("chunk").index_count, 6 + 3);
    }

    #[test]
    fn append_indices_creates_a_new_chunk_when_the_tail_is_full() {
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(4096)).expect("valid");
        let before = snapshot(&store, id);
        assert_eq!(before.chunks().len(), 1);
        let append_at = before.index_len();
        let first_vertex = (before.vertex_len() / 12) as u16;
        store
            .update_vertices(
                id,
                before.vertex_len()..before.vertex_len() + 36,
                &vertex_bytes(&[[100.0, 100.0], [101.0, 100.0], [100.0, 101.0]]),
            )
            .expect("追加顶点");
        let update = store
            .update_indices(
                id,
                append_at..append_at + 6,
                &index_bytes_u16(&[first_vertex, first_vertex + 1, first_vertex + 2]),
            )
            .expect("追加索引");
        let after = snapshot(&store, id);
        assert_eq!(after.chunks().len(), 2, "满块后追加必须新建块");
        let fresh = after.chunk(1).expect("chunk");
        assert_eq!((fresh.index_first, fresh.index_count), (12288, 3));
        assert_eq!(after.chunk(0).expect("chunk").index_count, 12288);
        assert!(
            update.damaged.max().x >= 101.0,
            "新块的 bounds 必须并进 damage：{:?}",
            update.damaged
        );
    }

    #[test]
    fn cross_chunk_index_update_damages_both_chunks() {
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(4096 + 10)).expect("valid");
        let before = snapshot(&store, id);
        // 覆盖第一块末尾与第二块开头的一段索引（跨越块边界）。
        let start = (GEOMETRY_CHUNK_INDICES - 6) * 2;
        let mut bytes = Vec::new();
        for index in 0..12u16 {
            // 用两块里已有的顶点（0 与 12288）交替，模拟跨块数据写入。
            bytes.extend_from_slice(&(index % 2).to_le_bytes());
        }
        let update = store
            .update_indices(id, start..start + bytes.len() as u32, &bytes)
            .expect("跨块索引更新");
        let after = snapshot(&store, id);
        assert_ne!(
            after.chunk(0).expect("chunk").revision,
            before.chunk(0).expect("chunk").revision
        );
        assert_ne!(
            after.chunk(1).expect("chunk").revision,
            before.chunk(1).expect("chunk").revision
        );
        assert!(!update.damaged.is_empty());
        assert_eq!(after.chunks().len(), 2);
    }

    #[test]
    fn triangle_strip_is_chunked_as_a_single_chunk() {
        let vertices = vertex_bytes(&[[0.0, 0.0], [1.0, 0.0], [2.0, 0.0], [3.0, 1.0]]);
        let mut store = GeometryStore::new();
        let id = store
            .insert(GeometryDesc {
                vertex_bytes: Bytes::from(vertices),
                index_bytes: Bytes::from(index_bytes_u16(&[0, 1, 2, 3])),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleStrip,
                bounds: Rect::new(0.0, 0.0, 3.0, 1.0),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("strip");
        let strip_snapshot = snapshot(&store, id);
        assert_eq!(strip_snapshot.chunks().len(), 1, "TriangleStrip 退化为单块");
        assert_eq!(strip_snapshot.chunk(0).expect("chunk").index_count, 4);
        // 追加索引仍在同一块内（index 4 ← 顶点 3）。
        let update = store
            .update_indices(id, 8..10, &3u16.to_le_bytes())
            .expect("strip 追加");
        assert!(update.damaged.max().x >= 3.0);
        assert_eq!(snapshot(&store, id).chunks().len(), 1);
    }

    #[test]
    fn snapshot_pages_and_chunks_follow_updates() {
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("valid");
        let first = snapshot(&store, id);
        let second = store
            .update_indices(id, 0..6, &index_bytes_u16(&[2, 1, 0]))
            .expect("update indices");
        let after = snapshot(&store, id);
        assert_eq!(after.revision(), second.revision);
        assert!(after.revision() > first.revision());
        // 索引页换新、顶点页共享。
        assert_ne!(
            after.index_pages()[0].as_ptr(),
            first.index_pages()[0].as_ptr()
        );
        assert_eq!(
            after.vertex_pages()[0].as_ptr(),
            first.vertex_pages()[0].as_ptr()
        );
        assert_eq!(after.chunk(0).expect("chunk").revision, second.revision);
    }

    #[test]
    fn chunk_bounds_enclose_the_vertices_and_never_collapse() {
        // 纯水平线：高度为 0，但块 bounds 必须仍有非零尺寸（否则 damage 为空）。
        let vertices = vertex_bytes(&[[0.0, 5.0], [10.0, 5.0], [20.0, 5.0]]);
        let mut store = GeometryStore::new();
        let id = store
            .insert(GeometryDesc {
                vertex_bytes: Bytes::from(vertices),
                index_bytes: Bytes::from(index_bytes_u16(&[0, 1, 2])),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds: Rect::new(0.0, 5.0, 20.0, 0.0),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("horizontal");
        let bounds = snapshot(&store, id).chunk(0).expect("chunk").bounds;
        assert!(!bounds.is_empty(), "水平线块的 bounds 不得为空");
        assert!(bounds.origin.x <= 0.0 && bounds.max().x >= 20.0);
        assert!(bounds.origin.y <= 5.0 && bounds.max().y >= 5.0);
        assert!(bounds.size.width >= 20.0);
    }

    #[test]
    fn released_slot_does_not_keep_the_old_snapshot_alive() {
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("valid");
        let snapshot = store.snapshot_arc(id).expect("snapshot");
        assert!(store.release(id));
        // 旧快照仍可用（在途帧持有 Arc），但 store 不再提供它。
        assert_eq!(snapshot.vertex_len(), 36);
        assert!(store.snapshot_arc(id).is_none());
    }

    // ── 截断：块数只减 / 旧块 damage / 页收缩 / 截断后追加 ───────────────────

    /// 4106 个三角形 = 2 块（块 1 只有 30 个索引）；截断到 4096 个三角形后只剩块 0，
    /// damage 必须覆盖被移除块 1 的旧 AABB，且块 0 没变 ⇒ 它的 revision 不推进。
    #[test]
    fn truncate_shrinks_chunks_and_damages_the_removed_blocks() {
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(4096 + 10)).expect("valid");
        let before = snapshot(&store, id);
        assert_eq!(before.chunks().len(), 2, "4106 三角形 = 两块");
        let removed = before.chunk(1).expect("chunk").bounds;
        let kept_revision = before.chunk(0).expect("chunk").revision;
        assert!(!removed.is_empty());

        let update = store.truncate(id, 4096 * 3, 4096 * 3).expect("成对截断");
        assert!(update.revision > before.revision(), "revision 必须前进");
        assert!(
            update.damaged.origin.x <= removed.origin.x
                && update.damaged.origin.y <= removed.origin.y
                && update.damaged.max().x >= removed.max().x
                && update.damaged.max().y >= removed.max().y,
            "damage 必须覆盖被移除块的旧 AABB：removed={removed:?} damaged={:?}",
            update.damaged
        );

        let after = snapshot(&store, id);
        assert_eq!(after.revision(), update.revision);
        assert_eq!(after.chunks().len(), 1, "块数只减");
        assert_eq!(after.chunk(0).expect("chunk").index_count, 4096 * 3);
        assert_eq!(
            after.chunk(0).expect("chunk").revision,
            kept_revision,
            "尾块完全没变 ⇒ 不推进 revision（paint 复用它的块 lane）"
        );
        assert_eq!(after.vertex_len(), 4096 * 3 * 12);
        assert_eq!(after.index_len(), 4096 * 3 * 2);
    }

    /// 末端页按新长度重写、多余页丢弃；未触碰的整页仍与旧快照共享同一份 `Bytes`。
    #[test]
    fn truncate_shortens_the_tail_page_and_drops_extra_pages() {
        // 500 三角形 = 18000 B 顶点（5 页：4 × 4096 + 1616）+ 3000 B 索引（1 页）。
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(500)).expect("valid");
        let before = snapshot(&store, id);
        assert_eq!(before.vertex_pages().len(), 5);
        assert_eq!(before.index_pages().len(), 1);
        let old_tail_page = Bytes::clone(&before.vertex_pages()[3]);
        assert_eq!(before.vertex_pages()[4].len(), 1616);
        let old_chunk_bounds = before.chunk(0).expect("chunk").bounds;

        // 400 三角形 = 14400 B 顶点（4 页：3 × 4096 + 2112）+ 2400 B 索引（1 页）。
        let update = store.truncate(id, 1200, 1200).expect("截断");
        let after = snapshot(&store, id);
        assert_eq!(after.vertex_len(), 14400);
        assert_eq!(after.index_len(), 2400);
        assert_eq!(after.vertex_pages().len(), 4, "多余页必须丢弃");
        assert_eq!(
            after.vertex_pages()[3].len(),
            14400 - 3 * GEOMETRY_PAGE_BYTES
        );
        assert_eq!(
            &after.vertex_pages()[3][..],
            &old_tail_page[..14400 - 3 * GEOMETRY_PAGE_BYTES],
            "新末页必须是旧末页的前缀（数据没被改写，只是变短）"
        );
        for page in 0..3 {
            assert_eq!(
                after.vertex_pages()[page].as_ptr(),
                before.vertex_pages()[page].as_ptr(),
                "未触碰的页必须共享同一份分配：page {page}"
            );
        }
        assert_eq!(after.index_pages().len(), 1);
        assert_eq!(after.index_pages()[0].len(), 2400);
        assert_eq!(after.chunks().len(), 1);
        assert_eq!(after.chunk(0).expect("chunk").index_count, 1200);
        assert_eq!(
            update.damaged, old_chunk_bounds,
            "damage = 变短的尾块旧 AABB ∪ 新 AABB = 旧 AABB"
        );
    }

    /// 截断后继续尾随追加：走现有的 append 路径，只动尾块；damage 覆盖追加的新位置。
    #[test]
    fn truncate_then_append_reuses_the_tail_append_path() {
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(4096 + 2)).expect("valid");
        // 先追加一个三角形（尾块 6 → 9 个索引）。
        let appended = snapshot(&store, id);
        let base = (appended.vertex_len() / 12) as u16;
        let vertex_at = appended.vertex_len();
        store
            .update_vertices(
                id,
                vertex_at..vertex_at + 36,
                &vertex_bytes(&[[200.0, 200.0], [201.0, 200.0], [200.0, 201.0]]),
            )
            .expect("追加顶点");
        let index_at = appended.index_len();
        store
            .update_indices(
                id,
                index_at..index_at + 6,
                &index_bytes_u16(&[base, base + 1, base + 2]),
            )
            .expect("追加索引");
        assert_eq!(snapshot(&store, id).chunk(1).expect("chunk").index_count, 9);
        let base_count = u32::from(base);

        // 截掉刚才那个三角形：尾块回到 6 个索引，damage 覆盖 (200,200) 的旧位置。
        let update = store.truncate(id, base_count, base_count).expect("截断");
        assert!(
            update.damaged.max().x >= 201.0 && update.damaged.max().y >= 201.0,
            "被截掉三角形的新位置必须进 damage：{:?}",
            update.damaged
        );
        let truncated = snapshot(&store, id);
        assert_eq!(truncated.chunks().len(), 2);
        assert_eq!(truncated.chunk(1).expect("chunk").index_count, 6);
        assert_eq!(truncated.vertex_len(), base_count * 12);
        assert_eq!(truncated.index_len(), base_count * 2);

        // 截断后再追加（同一路径）：尾块回到 9 个索引，新位置进 damage。
        let vertex_at = truncated.vertex_len();
        store
            .update_vertices(
                id,
                vertex_at..vertex_at + 36,
                &vertex_bytes(&[[300.0, 300.0], [301.0, 300.0], [300.0, 301.0]]),
            )
            .expect("再次追加顶点");
        let index_at = truncated.index_len();
        let update = store
            .update_indices(
                id,
                index_at..index_at + 6,
                &index_bytes_u16(&[base, base + 1, base + 2]),
            )
            .expect("再次追加索引");
        let again = snapshot(&store, id);
        assert_eq!(again.chunks().len(), 2);
        assert_eq!(again.chunk(1).expect("chunk").index_count, 9);
        assert!(
            update.damaged.max().x >= 301.0,
            "再次追加的新位置必须进 damage：{:?}",
            update.damaged
        );
        assert!(
            again.chunk(1).expect("chunk").bounds.max().x >= 301.0,
            "尾块 AABB 必须包含新三角形：{:?}",
            again.chunk(1).expect("chunk").bounds
        );
    }

    /// 截断删掉尾块后，下一次追加必须能重新建出块 1（块表不能残留在「已删除」状态）。
    #[test]
    fn append_after_truncation_creates_the_next_chunk_again() {
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(4096 + 2)).expect("valid");
        let update = store.truncate(id, 4096 * 3, 4096 * 3).expect("截断到一块");
        assert_eq!(snapshot(&store, id).chunks().len(), 1);
        assert!(!update.damaged.is_empty(), "被移除的块 1 必须进 damage");

        let truncated = snapshot(&store, id);
        let vertex_at = truncated.vertex_len();
        store
            .update_vertices(
                id,
                vertex_at..vertex_at + 36,
                &vertex_bytes(&[[100.0, 100.0], [101.0, 100.0], [100.0, 101.0]]),
            )
            .expect("追加顶点");
        let index_at = truncated.index_len();
        let update = store
            .update_indices(
                id,
                index_at..index_at + 6,
                &index_bytes_u16(&[12288, 12289, 12290]),
            )
            .expect("追加索引");
        let after = snapshot(&store, id);
        assert_eq!(after.chunks().len(), 2, "被截掉的尾块必须能重新建出来");
        assert_eq!(
            (
                after.chunk(1).expect("chunk").index_first,
                after.chunk(1).expect("chunk").index_count
            ),
            (12288, 3)
        );
        assert!(
            update.damaged.max().x >= 101.0,
            "新块的 bounds 必须进 damage：{:?}",
            update.damaged
        );
    }

    /// 截断到 0：全部块消失、页表清空；之后可以从空缓冲重新增长回一块。
    #[test]
    fn truncate_to_zero_drops_every_chunk_and_can_grow_again() {
        let mut store = GeometryStore::new();
        let id = store.insert(long_desc(4096 + 2)).expect("valid");
        let before = snapshot(&store, id);
        assert_eq!(before.chunks().len(), 2);
        let update = store.truncate(id, 0, 0).expect("清空");
        assert!(!update.damaged.is_empty(), "被移除的全部块都必须进 damage");
        let empty = snapshot(&store, id);
        assert_eq!(empty.chunks().len(), 0);
        assert_eq!(empty.vertex_len(), 0);
        assert_eq!(empty.index_len(), 0);
        assert!(empty.vertex_pages().is_empty());
        assert!(empty.index_pages().is_empty());

        // 空缓冲上重新追加一个三角形。
        store
            .update_vertices(
                id,
                0..36,
                &vertex_bytes(&[[7.0, 7.0], [8.0, 7.0], [7.0, 8.0]]),
            )
            .expect("从空缓冲追加顶点");
        store
            .update_indices(id, 0..6, &index_bytes_u16(&[0, 1, 2]))
            .expect("从空缓冲追加索引");
        let after = snapshot(&store, id);
        assert_eq!(after.chunks().len(), 1);
        assert_eq!(after.chunk(0).expect("chunk").index_count, 3);
        assert_eq!(after.vertex_len(), 36);
        let _ = before;
    }

    /// 目标个数大于当前个数是**独立的强类型错误**（处理策略是换 API，不是修 range）。
    #[test]
    fn truncate_growth_is_a_typed_error() {
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("valid");
        let before = snapshot(&store, id);
        match store.truncate(id, 4, 3) {
            Err(EngineError::GeometryTruncateGrows {
                geometry,
                buffer,
                requested,
                current,
            }) => {
                assert_eq!(geometry, id);
                assert_eq!(buffer, GeometryBuffer::Vertex);
                assert_eq!((requested, current), (4, 3));
            }
            other => panic!("顶点增长必须报 GeometryTruncateGrows，实际 {other:?}"),
        }
        match store.truncate(id, 3, 4) {
            Err(EngineError::GeometryTruncateGrows { buffer, .. }) => {
                assert_eq!(buffer, GeometryBuffer::Index);
            }
            other => panic!("索引增长必须报 GeometryTruncateGrows，实际 {other:?}"),
        }
        // 失败不得改动任何状态（版本、长度、快照都保持）。
        let after = snapshot(&store, id);
        assert_eq!(after.revision(), before.revision());
        assert_eq!(after.vertex_len(), 36);
        assert_eq!(after.index_len(), 6);

        let stale = crate::paint::geometry::GeometryId::for_test(99, 1);
        match store.truncate(stale, 0, 0) {
            Err(EngineError::UnknownGeometry { geometry }) => assert_eq!(geometry, stale),
            other => panic!("陈旧句柄必须报 UnknownGeometry，实际 {other:?}"),
        }
    }

    /// 截断到当前长度 = no-op：不推进版本、不产生 damage、不重建快照。
    #[test]
    fn truncate_to_the_current_lengths_is_a_noop() {
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("valid");
        let before = snapshot(&store, id);
        let update = store.truncate(id, 3, 3).expect("等长截断是 no-op");
        assert_eq!(update.revision, before.revision(), "no-op 不推进版本");
        assert!(update.damaged.is_empty(), "no-op 不产生 damage");
        let after = snapshot(&store, id);
        assert!(
            std::sync::Arc::ptr_eq(&before, &after),
            "no-op 不得重建快照（GPU 侧没有任何可上传的差异）"
        );
    }
}
