use std::sync::Arc;

use bytes::Bytes;
use smallvec::SmallVec;

use crate::paint::{
    geometry::{GeometryId, GeometrySnapshot},
    instance::GeometryDrawGpu,
};

pub(crate) const GEOMETRY_BUFFER_GROWTH_ALIGN: u32 = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct GeometryPageUpload {
    pub page: u32,
    pub bytes: Bytes,
}

#[derive(Clone, Debug)]
pub(crate) struct GeometryBufferPlan {
    pub full: bool,
    pub realloc: bool,
    pub capacity: u32,
    pub pages: SmallVec<[GeometryPageUpload; 8]>,
}

impl GeometryBufferPlan {
    pub(crate) fn byte_len(&self) -> usize {
        self.pages.iter().map(|page| page.bytes.len()).sum()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GeometryUploadPlan {
    pub id: GeometryId,
    pub snapshot: Arc<GeometrySnapshot>,
    pub vertex: GeometryBufferPlan,
    pub index: GeometryBufferPlan,
}

impl GeometryUploadPlan {
    pub(crate) fn uploaded_bytes(&self) -> usize {
        self.vertex.byte_len() + self.index.byte_len()
    }
}

struct UploadState {
    snapshot: Arc<GeometrySnapshot>,
    vertex_capacity: u32,
    index_capacity: u32,
}

#[derive(Default)]
pub(crate) struct GeometryUploadTracker {
    states: ahash::HashMap<GeometryId, UploadState>,
    best: ahash::HashMap<GeometryId, usize>,
    seen: Vec<GeometryId>,
}

impl GeometryUploadTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn clear(&mut self) {
        self.states.clear();
    }

    pub(crate) fn plan(&mut self, geometry_table: &[GeometryDrawGpu]) -> Vec<GeometryUploadPlan> {
        if geometry_table.is_empty() {
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

    fn plan_one(
        &mut self,
        id: GeometryId,
        snapshot: Arc<GeometrySnapshot>,
    ) -> Option<GeometryUploadPlan> {
        let Some(state) = self.states.get_mut(&id) else {
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

fn full_plan(pages: &[Bytes], len: u32, current_capacity: u32) -> GeometryBufferPlan {
    let capacity = growth_capacity(len, current_capacity);
    full_plan_with_capacity(pages, capacity)
}

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

fn growth_capacity(len: u32, current_capacity: u32) -> u32 {
    if current_capacity > 0 && len <= current_capacity {
        return current_capacity;
    }
    len.max(GEOMETRY_BUFFER_GROWTH_ALIGN)
        .div_ceil(GEOMETRY_BUFFER_GROWTH_ALIGN)
        * GEOMETRY_BUFFER_GROWTH_ALIGN
}

fn same_page(a: &Bytes, b: &Bytes) -> bool {
    a.len() == b.len() && (a.is_empty() || a.as_ptr() == b.as_ptr())
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StagingLayout {
    pub total_bytes: usize,
    pub offsets: Vec<usize>,
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
pub(crate) const STAGING_ALIGN: usize = 512;

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
pub(crate) fn staging_layout(page_lens: impl IntoIterator<Item = usize>) -> StagingLayout {
    let mut total_bytes = 0usize;
    let mut offsets = Vec::new();
    for len in page_lens {
        offsets.push(total_bytes);
        total_bytes += aligned_page_len(len);
    }
    StagingLayout {
        total_bytes,
        offsets,
    }
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
impl StagingLayout {
    #[must_use]
    pub(crate) fn fits(&self, page: usize, len: usize) -> bool {
        self.offsets
            .get(page)
            .is_some_and(|offset| offset + len <= self.total_bytes)
    }
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn aligned_page_len(len: usize) -> usize {
    len.div_ceil(STAGING_ALIGN) * STAGING_ALIGN
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;

    use super::{
        GEOMETRY_BUFFER_GROWTH_ALIGN, GeometryUploadTracker, STAGING_ALIGN, growth_capacity,
        same_page, staging_layout,
    };
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
        let range = 0..snapshot.index_count();
        GeometryDrawGpu {
            id,
            range,
            transform: Transform::IDENTITY,
            color: [1.0; 4],
            snapshot,
        }
    }

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

    #[test]
    fn only_touched_pages_are_uploaded() {
        let triangles = 400;
        let (mut store, id) = fixture(triangles);
        let snapshot = store.snapshot_arc(id).expect("快照");
        assert!(snapshot.vertex_pages().len() >= 4);
        let mut tracker = GeometryUploadTracker::new();
        let first = tracker.plan(&[draw(id, Arc::clone(&snapshot))]);
        assert!(first[0].vertex.full);
        let capacity = first[0].vertex.capacity;

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

    #[test]
    fn growth_plan_is_full_and_aligned() {
        let (mut store, id) = fixture(2);
        let snapshot = store.snapshot_arc(id).expect("快照");
        let mut tracker = GeometryUploadTracker::new();
        let first = tracker.plan(&[draw(id, Arc::clone(&snapshot))]);
        let mut capacity = first[0].vertex.capacity;
        assert_eq!(capacity, GEOMETRY_BUFFER_GROWTH_ALIGN);

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

    #[test]
    fn staging_layout_fits_unaligned_pages_across_plan_boundaries() {
        let layout = staging_layout([36, 6]);
        assert_eq!(layout.total_bytes, 1024);
        assert_eq!(layout.offsets, vec![0, 512]);
        assert!(layout.fits(0, 36));
        assert!(layout.fits(1, 6));
        assert!(layout.total_bytes >= 512 + 6, "index 页必须整段落在缓冲内");
        assert!(!layout.fits(1, 513), "越过缓冲末尾的写入必须被判为放不下");
    }

    #[test]
    fn staging_layout_single_unaligned_page() {
        let layout = staging_layout([36]);
        assert_eq!(layout.total_bytes, STAGING_ALIGN);
        assert_eq!(layout.offsets, vec![0]);
        assert!(layout.fits(0, 36));
    }

    #[test]
    fn staging_layout_mixed_pages_respect_offset_and_length() {
        let layout = staging_layout([4096, 768, 744]);
        assert_eq!(layout.offsets, vec![0, 4096, 5120]);
        assert_eq!(layout.total_bytes, 4096 + 1024 + 1024);
        for (page, len) in [4096, 768, 744].into_iter().enumerate() {
            assert!(layout.fits(page, len), "第 {page} 页越界");
            assert_eq!(
                layout.offsets[page] % STAGING_ALIGN,
                0,
                "每页偏移必须 512 B 对齐"
            );
        }
        assert!(staging_layout(Vec::<usize>::new()).offsets.is_empty());
        assert_eq!(staging_layout(Vec::<usize>::new()).total_bytes, 0);
    }
}
