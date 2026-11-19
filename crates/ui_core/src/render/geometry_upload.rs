//! 静态几何快照的**页级上传计划**（X5）：三端 GPU 后端与合成端计数共用的唯一 diff 算法。
//!
//! # 为什么需要计划
//!
//! `GeometrySnapshot` 的顶点/索引页是不可变 `Bytes`：一次 range 更新只替换被触碰的页，未变的
//! 页与上一版本**共享同一份分配**。因此「这一帧要上传多少几何数据」= 对每个几何取最新的快照、
//! 逐页比较指针（`ptr_eq`），变化的页才进计划；缓冲增长（长度超过已分配容量）时整份重传并
//! 按 [`GEOMETRY_BUFFER_GROWTH_ALIGN`] 对齐扩容。
//!
//! 与成熟实现的对应：
//! - Qt `QSGGeometry` 的 `DirtyGeometry` 只重传脏顶点区间 —— 本模块把「脏区间」量化到页；
//! - Chrome 的 GPU 资源复制（`ResourceProvider` 的 staging + CopyBuffer）同样按脏区域判断；
//!   三端后端的差别只是「怎么 copy」：D3D12 `CopyBufferRegion` / Metal blit /
//!   Vulkan `vkCmdCopyBuffer`。
//!
//! # 状态与丢帧
//!
//! Tracker 的「已上传」状态在**生成计划时推进**。合成线程用它做 CPU 侧计数（headless 场景
//! 无丢帧，与实际 GPU 上传一致）；真实窗口的 GPU 线程各持自己的 tracker（渲染器重建时
//! `clear()` ⇒ 下一帧整份重传）。两处算法同一实现，只差调用者。

use std::sync::Arc;

use bytes::Bytes;
use smallvec::SmallVec;

use crate::paint::{
    geometry::{GeometryId, GeometrySnapshot},
    instance::GeometryDrawGpu,
};

/// GPU 几何缓冲容量增长的对齐粒度（64 KiB）。
///
/// `width` 型缓冲在 64 KiB 边界上重新分配后，K 线逐根追加（每根 24 B 索引 + 少量顶点字节）
/// 约 2700 次才触发一次扩容；扩容 = 新建缓冲 + 整份页上传，因此必须摊薄到可忽略的频率。
/// 三端后端分配容量时**必须**用同一取整（否则 CPU 侧计数与 GPU 侧实际字节会漂移）。
pub(crate) const GEOMETRY_BUFFER_GROWTH_ALIGN: u32 = 64 * 1024;

/// 一页上传（页号 + 该页字节；`Bytes` 与新快照共享分配，不复制）。
#[derive(Clone, Debug)]
pub(crate) struct GeometryPageUpload {
    /// 页号（页偏移 = `page × GEOMETRY_PAGE_BYTES`）。
    pub page: u32,
    pub bytes: Bytes,
}

/// 一个缓冲（顶点或索引）的上传计划。
#[derive(Clone, Debug)]
pub(crate) struct GeometryBufferPlan {
    /// 是否**整份**上传（首见 / 容量不足 ⇒ 全部页）。`true` 时后端的旧缓冲内容可以丢弃
    /// （若同时 `realloc`，旧缓冲直接 retire 并新建）。
    pub full: bool,
    /// 容量不足 ⇒ 必须按 `capacity` 重新分配 GPU 缓冲。
    pub realloc: bool,
    /// 本次计划的缓冲容量（≥ 页总长；`realloc == false` 时 = 当前容量）。
    pub capacity: u32,
    /// 要上传的页（`full == true` 时是全部页）。
    pub pages: SmallVec<[GeometryPageUpload; 8]>,
}

impl GeometryBufferPlan {
    /// 本计划的上传字节数。
    pub(crate) fn byte_len(&self) -> usize {
        self.pages.iter().map(|page| page.bytes.len()).sum()
    }
}

/// 一个几何（id + generation）本帧的上传计划。
#[derive(Clone, Debug)]
pub(crate) struct GeometryUploadPlan {
    pub id: GeometryId,
    pub snapshot: Arc<GeometrySnapshot>,
    pub vertex: GeometryBufferPlan,
    pub index: GeometryBufferPlan,
}

impl GeometryUploadPlan {
    /// 该几何本帧的上传字节数（顶点页 + 索引页）。
    pub(crate) fn uploaded_bytes(&self) -> usize {
        self.vertex.byte_len() + self.index.byte_len()
    }
}

/// 每个已上传几何的跨帧状态。
struct UploadState {
    snapshot: Arc<GeometrySnapshot>,
    vertex_capacity: u32,
    index_capacity: u32,
}

/// 页级上传计划器（每个消费者一份：合成端计数 / 各 GPU 后端）。
#[derive(Default)]
pub(crate) struct GeometryUploadTracker {
    states: ahash::HashMap<GeometryId, UploadState>,
    /// 本帧每个几何「最新版本快照」所在的表下标（工作区，跨帧复用）。
    best: ahash::HashMap<GeometryId, usize>,
    /// 上一帧引用到的几何 id（驱逐用；工作区）。
    seen: Vec<GeometryId>,
}

impl GeometryUploadTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 丢弃全部状态（渲染器重建 / 设备恢复 / DSR：下一帧必须整份重传）。
    pub(crate) fn clear(&mut self) {
        self.states.clear();
    }

    /// 生成一帧的上传计划（按 `geometry_table` 里的**最新**快照做页级 diff）。
    ///
    /// 同一几何可能在一帧的几何表里出现多次（多个块 lane / 复用段保留旧快照）：只取
    /// revision 最大的那个 —— 旧表项画的是新索引缓冲的合法前缀（块在索引缓冲里连续、
    /// 长度只增不减），因此用最新快照上传、按各表项自己的块区间绘制是正确的。
    pub(crate) fn plan(&mut self, geometry_table: &[GeometryDrawGpu]) -> Vec<GeometryUploadPlan> {
        if geometry_table.is_empty() {
            // 本帧没有几何绘制：驱逐全部状态（窗口空闲 / 几何全部隐藏时不让快照常驻）。
            // 再次引用时按首见整份上传 —— 这只是「重新出现」的一次性成本。
            self.states.clear();
            return Vec::new();
        }
        self.best.clear();
        for (index, draw) in geometry_table.iter().enumerate() {
            match self.best.get(&draw.id) {
                Some(existing)
                    if draw.snapshot.revision()
                        <= geometry_table[*existing].snapshot.revision() => {}
                _ => {
                    self.best.insert(draw.id, index);
                }
            }
        }
        // 驱逐本帧不再引用的几何（应用 release / 节点卸载后状态不常驻）。
        self.seen.clear();
        self.seen.extend(self.best.keys().copied());
        self.states.retain(|id, _| self.best.contains_key(id));

        let mut plans = Vec::with_capacity(self.best.len());
        let ids: SmallVec<[GeometryId; 8]> = self.seen.iter().copied().collect();
        for id in &ids {
            let index = self.best[id];
            let snapshot = Arc::clone(&geometry_table[index].snapshot);
            if let Some(plan) = self.plan_one(*id, snapshot) {
                plans.push(plan);
            }
        }
        plans
    }

    /// 单个几何的 diff：与上次状态比较 revision 与页指针。
    fn plan_one(
        &mut self,
        id: GeometryId,
        snapshot: Arc<GeometrySnapshot>,
    ) -> Option<GeometryUploadPlan> {
        let Some(state) = self.states.get_mut(&id) else {
            // 首见：整份上传，容量按对齐粒度分配。
            let vertex = full_plan(snapshot.vertex_pages(), snapshot.vertex_len(), 0);
            let index = full_plan(snapshot.index_pages(), snapshot.index_len(), 0);
            self.states.insert(
                id,
                UploadState {
                    snapshot: Arc::clone(&snapshot),
                    vertex_capacity: vertex.capacity,
                    index_capacity: index.capacity,
                },
            );
            return Some(GeometryUploadPlan {
                id,
                snapshot,
                vertex,
                index,
            });
        };
        if snapshot.revision() <= state.snapshot.revision() {
            // 旧快照（复用段）或不比已上传的新：没有上传工作。
            return None;
        }
        let vertex = plan_buffer(
            snapshot.vertex_pages(),
            state.snapshot.vertex_pages(),
            snapshot.vertex_len(),
            &mut state.vertex_capacity,
        );
        let index = plan_buffer(
            snapshot.index_pages(),
            state.snapshot.index_pages(),
            snapshot.index_len(),
            &mut state.index_capacity,
        );
        state.snapshot = Arc::clone(&snapshot);
        Some(GeometryUploadPlan {
            id,
            snapshot,
            vertex,
            index,
        })
    }
}

/// 首见 / 重建：整份上传 + 对齐容量。
fn full_plan(pages: &[Bytes], len: u32, current_capacity: u32) -> GeometryBufferPlan {
    let capacity = growth_capacity(len, current_capacity);
    full_plan_with_capacity(pages, capacity)
}

/// 已定容量的整份上传（`plan_buffer` 的扩容路径把新容量写回状态后复用）。
fn full_plan_with_capacity(pages: &[Bytes], capacity: u32) -> GeometryBufferPlan {
    GeometryBufferPlan {
        full: true,
        realloc: true,
        capacity,
        pages: pages
            .iter()
            .enumerate()
            .map(|(page, bytes)| GeometryPageUpload {
                page: page as u32,
                bytes: Bytes::clone(bytes),
            })
            .collect(),
    }
}

/// 增量：只上传页指针变化的页；长度超出容量时退回整份。
fn plan_buffer(
    pages: &[Bytes],
    old_pages: &[Bytes],
    len: u32,
    capacity: &mut u32,
) -> GeometryBufferPlan {
    if len > *capacity {
        *capacity = growth_capacity(len, *capacity);
        return full_plan_with_capacity(pages, *capacity);
    }
    let mut uploads = SmallVec::with_capacity(2);
    for (page, bytes) in pages.iter().enumerate() {
        let changed = old_pages.get(page).is_none_or(|old| !same_page(old, bytes));
        if changed {
            uploads.push(GeometryPageUpload {
                page: page as u32,
                bytes: Bytes::clone(bytes),
            });
        }
    }
    GeometryBufferPlan {
        full: false,
        realloc: false,
        capacity: *capacity,
        pages: uploads,
    }
}

/// 容量取整：至少一个对齐单位、按 [`GEOMETRY_BUFFER_GROWTH_ALIGN`] 向上取整。
fn growth_capacity(len: u32, current_capacity: u32) -> u32 {
    if current_capacity > 0 && len <= current_capacity {
        return current_capacity;
    }
    len.max(GEOMETRY_BUFFER_GROWTH_ALIGN)
        .div_ceil(GEOMETRY_BUFFER_GROWTH_ALIGN)
        * GEOMETRY_BUFFER_GROWTH_ALIGN
}

/// 两页内容是否同一份分配（未变的页共享 `Bytes`；比较指针 + 长度即可）。
fn same_page(a: &Bytes, b: &Bytes) -> bool {
    a.len() == b.len() && (a.is_empty() || a.as_ptr() == b.as_ptr())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;

    use super::{GEOMETRY_BUFFER_GROWTH_ALIGN, GeometryUploadTracker, growth_capacity, same_page};
    use crate::{
        math::{Rect, Transform},
        paint::{
            geometry::{
                GEOMETRY_PAGE_BYTES, GeometryDesc, GeometryId, GeometryPipeline, IndexType,
                Topology,
            },
            instance::GeometryDrawGpu,
        },
    };

    /// 注册一个三角形几何，返回 store 与快照句柄（测试只关心快照）。
    fn fixture(triangles: u32) -> (crate::paint::geometry_store::GeometryStore, GeometryId) {
        let vertex_len = triangles as usize * 36;
        let index_len = triangles as usize * 6;
        let mut vertices = vec![0u8; vertex_len];
        let mut indices = Vec::with_capacity(index_len);
        for triangle in 0..triangles {
            let base = (triangle * 3) as u16;
            for index in [base, base + 1, base + 2] {
                indices.extend_from_slice(&index.to_ne_bytes());
            }
            // 真实位置：三角形按 (x,y) 排布，供块 AABB 扫描。
            for offset in 0..3 {
                let at = (triangle as usize * 3 + offset) * 12;
                vertices[at..at + 4].copy_from_slice(&((triangle % 64) as f32).to_ne_bytes());
                vertices[at + 4..at + 8]
                    .copy_from_slice(&((triangle / 64) as f32 + offset as f32).to_ne_bytes());
            }
        }
        let mut store = crate::paint::geometry_store::GeometryStore::new();
        let id = store
            .insert(GeometryDesc {
                vertex_bytes: Bytes::from(vertices),
                index_bytes: Bytes::from(indices),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds: Rect::new(0.0, 0.0, 64.0, 100.0),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册几何");
        (store, id)
    }

    fn draw(
        id: GeometryId,
        snapshot: Arc<crate::paint::geometry::GeometrySnapshot>,
    ) -> GeometryDrawGpu {
        GeometryDrawGpu {
            id,
            chunk: 0,
            transform: Transform::IDENTITY,
            color: [1.0; 4],
            snapshot,
        }
    }

    /// 首见整份上传；同一版本再 planning 不产生上传。
    #[test]
    fn first_plan_uploads_everything_then_stays_quiet() {
        let (store, id) = fixture(1);
        let snapshot = store.snapshot_arc(id).expect("快照");
        let mut tracker = GeometryUploadTracker::new();
        let plans = tracker.plan(&[draw(id, Arc::clone(&snapshot))]);
        assert_eq!(plans.len(), 1);
        assert!(plans[0].vertex.full && plans[0].vertex.realloc);
        assert_eq!(plans[0].vertex.byte_len(), 36);
        assert_eq!(plans[0].index.byte_len(), 6);
        let first = plans[0].uploaded_bytes();
        assert!(first > 0);

        let plans = tracker.plan(&[draw(id, snapshot)]);
        assert!(plans.is_empty(), "同一版本不产生上传");
    }

    /// 一次 range 更新只上传被触碰的页（4 KiB 粒度）。
    #[test]
    fn only_touched_pages_are_uploaded() {
        // 4 页顶点（≥ 4 × 4096 B）+ 1 页索引。
        let triangles = 400; // 14400 B 顶点 = 4 页
        let (mut store, id) = fixture(triangles);
        let snapshot = store.snapshot_arc(id).expect("快照");
        assert!(snapshot.vertex_pages().len() >= 4);
        let mut tracker = GeometryUploadTracker::new();
        let first = tracker.plan(&[draw(id, Arc::clone(&snapshot))]);
        assert!(first[0].vertex.full);
        let capacity = first[0].vertex.capacity;

        // 更新第 1 页里的一个顶点（offset 4104 是 12 的倍数且落在第 1 页）。
        let offset = 4104u32;
        store
            .update_vertices(id, offset..offset + 12, &[7u8; 12])
            .expect("更新");
        let snapshot = store.snapshot_arc(id).expect("新快照");
        let plans = tracker.plan(&[draw(id, snapshot)]);
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].vertex.realloc, "容量足够时不得扩容");
        assert!(!plans[0].vertex.full);
        assert_eq!(plans[0].vertex.capacity, capacity);
        assert_eq!(plans[0].vertex.pages.len(), 1, "只上传被触碰的 1 页");
        assert_eq!(plans[0].vertex.pages[0].page, 1);
        assert_eq!(plans[0].index.pages.len(), 0, "索引没变 ⇒ 不上传");
        assert!(plans[0].uploaded_bytes() <= GEOMETRY_PAGE_BYTES);
    }

    /// 追加越过容量 ⇒ 整份重传 + 按对齐粒度扩容；之后的小更新回到页级。
    #[test]
    fn growth_plan_is_full_and_aligned() {
        let (mut store, id) = fixture(2);
        let snapshot = store.snapshot_arc(id).expect("快照");
        let mut tracker = GeometryUploadTracker::new();
        let first = tracker.plan(&[draw(id, Arc::clone(&snapshot))]);
        let mut capacity = first[0].vertex.capacity;
        assert_eq!(capacity, GEOMETRY_BUFFER_GROWTH_ALIGN);

        // 追加顶点直到超过容量（容量对齐，多半不会触发；这里直接构造超过容量的增长）。
        let snapshot = store.snapshot_arc(id).expect("快照");
        let vertex_at = snapshot.vertex_len();
        let unaligned = capacity - vertex_at + 12;
        let big_len = unaligned.div_ceil(12) * 12;
        let big = vec![0u8; big_len as usize];
        store
            .update_vertices(id, vertex_at..vertex_at + big.len() as u32, &big)
            .expect("增长");
        let grown = store.snapshot_arc(id).expect("快照");
        let plans = tracker.plan(&[draw(id, grown)]);
        assert_eq!(plans.len(), 1);
        assert!(plans[0].vertex.realloc && plans[0].vertex.full);
        assert!(plans[0].vertex.capacity > capacity);
        assert_eq!(plans[0].vertex.capacity % GEOMETRY_BUFFER_GROWTH_ALIGN, 0);
        capacity = plans[0].vertex.capacity;

        // 扩容后的小更新只上传变化的页。
        let before = store.snapshot_arc(id).expect("快照");
        store.update_indices(id, 0..2, &[0u8, 0]).expect("索引更新");
        let after = store.snapshot_arc(id).expect("快照");
        assert!(after.index_pages().len() <= before.index_pages().len());
        let plans = tracker.plan(&[draw(id, after)]);
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].index.full && !plans[0].index.realloc);
        assert_eq!(plans[0].index.pages.len(), 1);
        assert_eq!(plans[0].vertex.pages.len(), 0, "顶点没变 ⇒ 不上传");
        assert_eq!(plans[0].vertex.capacity, capacity);
    }

    /// 同一几何的多个表项（复用段 + 新块）只按**最新**快照规划一次。
    #[test]
    fn duplicate_entries_use_the_newest_snapshot_once() {
        let (mut store, id) = fixture(1);
        let old = store.snapshot_arc(id).expect("旧快照");
        let mut tracker = GeometryUploadTracker::new();
        assert_eq!(
            tracker.plan(&[draw(id, Arc::clone(&old))]).len(),
            1,
            "首见一次"
        );
        store.update_vertices(id, 0..12, &[3u8; 12]).expect("更新");
        let new = store.snapshot_arc(id).expect("新快照");
        let plans = tracker.plan(&[draw(id, Arc::clone(&old)), draw(id, Arc::clone(&new))]);
        assert_eq!(plans.len(), 1, "同 id 一帧只产生一个计划");
        assert_eq!(plans[0].snapshot.revision(), new.revision());
        assert!(!plans[0].vertex.full);
        assert!(!plans[0].vertex.pages.is_empty(), "变化页必须上传");
    }

    /// 只带旧快照（复用段）的帧不产生上传。
    #[test]
    fn stale_snapshot_only_frames_upload_nothing() {
        let (mut store, id) = fixture(1);
        let mut tracker = GeometryUploadTracker::new();
        let first = store.snapshot_arc(id).expect("快照");
        tracker.plan(&[draw(id, Arc::clone(&first))]);
        store.update_vertices(id, 0..12, &[1u8; 12]).expect("更新");
        assert!(
            tracker.plan(&[draw(id, first)]).is_empty(),
            "旧版本快照不产生上传"
        );
    }

    /// 驱逐：几何不再被引用后状态释放（again 首见 ⇒ 整份上传）。
    #[test]
    fn unreferenced_geometry_state_is_evicted() {
        let (store, id) = fixture(1);
        let snapshot = store.snapshot_arc(id).expect("快照");
        let mut tracker = GeometryUploadTracker::new();
        assert_eq!(tracker.plan(&[draw(id, Arc::clone(&snapshot))]).len(), 1);
        assert!(tracker.plan(&[]).is_empty());
        assert_eq!(tracker.states.len(), 0, "无几何绘制的帧会驱逐状态");
        let plans = tracker.plan(&[draw(id, snapshot)]);
        assert!(plans[0].vertex.full, "驱逐后再次引用必须整份上传");
    }

    #[test]
    fn page_identity_and_growth_capacity_are_exact() {
        assert!(same_page(&Bytes::new(), &Bytes::new()));
        let base = Bytes::from(vec![1u8; 8]);
        assert!(same_page(&base, &base.clone()));
        let other = Bytes::from(vec![1u8; 8]);
        assert!(!same_page(&base, &other));
        assert_eq!(growth_capacity(0, 0), GEOMETRY_BUFFER_GROWTH_ALIGN);
        assert_eq!(
            growth_capacity(GEOMETRY_BUFFER_GROWTH_ALIGN + 1, 0),
            2 * GEOMETRY_BUFFER_GROWTH_ALIGN
        );
        assert_eq!(
            growth_capacity(10, GEOMETRY_BUFFER_GROWTH_ALIGN),
            GEOMETRY_BUFFER_GROWTH_ALIGN,
            "容量足够时不扩容"
        );
    }
}
