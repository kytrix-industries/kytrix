//! 静态几何的注册与生命周期：`GeometryStore`（P0-1.2）。
//!
//! # 为什么需要它
//!
//! `GeometryId` 的字段是私有的，唯一合法的铸造点是 [`GeometryStore`]。在此之前只有
//! `#[cfg(any(test, feature = "test-support"))]` 的 `GeometryId::for_test`，所以
//! `NodeBuilder::geometry` / `Canvas::push_path` 这些公开 API 在生产构建里**无法合法使用** ——
//! 图表几何通道因此从未被真正接通。
//!
//! # 与成熟实现的关系
//!
//! 语义对齐 Skia 的 `GrDirectContext::createBackendTexture` + `drawVertices`（几何数据一次性
//! 上传、绘制期只传变换与颜色）与 Qt 的 `QSGGeometry`（静态几何 + `markDirty(DirtyGeometry)`
//! 局部更新）。与它们一样，本 store 只负责**数据与生命周期**，不含任何 GPU 句柄：
//!
//! ```text
//! 应用线程                    GeometryStore（CPU）              GPU 线程
//! ─────────                   ───────────────────              ────────
//! insert(desc) ──► GeometryId  槽位 + 代际 + Bytes
//!                              │
//! Primitive::Geometry(id) ─────┘   （每帧只传 Copy 句柄）
//!                                            └─► 首次见到该 id 时上传顶点/索引缓冲
//! release(id) ─────────────────► 槽位释放（代际 +1，旧句柄立即失效）
//!                                            └─► GPU 侧按代际淘汰缓冲
//! ```
//!
//! # 代际校验
//!
//! 槽位复用后代际 +1，旧句柄永久失效。渲染层查表时必须**同时**比对 index 与 generation
//! （各后端的 `GeometryRegistry` 缓存键就是 `(index, generation)`），否则释放后再注册会让旧图元
//! 画成新几何 —— 这是静默的视觉错误，不是崩溃。

use smol_str::{SmolStr, format_smolstr};

use crate::{
    error::EngineError,
    node::id::Arena,
    paint::geometry::{GeometryDesc, GeometryId},
};

/// 已注册几何的 CPU 侧副本（GPU 缓冲由渲染层按需创建并缓存）。
#[derive(Debug)]
pub(crate) struct GeometryRecord {
    /// 注册描述（顶点/索引字节、布局、拓扑、AABB、管线）。
    pub desc: std::sync::Arc<GeometryDesc>,
}

/// 静态几何的进程级存储（`App` 持有一份；与窗口无关）。
///
/// 几何数据是**设备无关**的 CPU 字节：同一个 `GeometryId` 在所有窗口/所有 GPU 后端上都有效，
/// 各后端自行缓存自己的顶点/索引缓冲（单设备多窗口时天然共享）。
pub struct GeometryStore {
    entries: Arena<GeometryRecord, GeometryId>,
    /// 代际计数器：`Arena` 自己维护每个槽的代际，这里只记账统计用的峰值。
    peak: usize,
}

impl GeometryStore {
    /// 空 store（消费者：`App::new`）。
    pub(crate) fn new() -> Self {
        Self {
            entries: Arena::new(),
            peak: 0,
        }
    }

    /// 注册一份静态几何，返回句柄。
    ///
    /// 校验（全部是**应用错误**，返回 `Err` 而不是 panic）：
    /// - `vertex_stride` 必须等于 `pipeline.vertex_stride()`；
    /// - `vertex_bytes.len()` 必须是 `vertex_stride` 的整数倍且非空；
    /// - `index_bytes.len()` 必须是 `index_type.size()` 的整数倍且非空；
    /// - `bounds` 必须是有限且非负尺寸（damage / 命中只用它）。
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
        if !desc.bounds.origin.x.is_finite()
            || !desc.bounds.origin.y.is_finite()
            || !desc.bounds.size.width.is_finite()
            || !desc.bounds.size.height.is_finite()
            || desc.bounds.size.width < 0.0
            || desc.bounds.size.height < 0.0
        {
            return Err(register_error(SmolStr::new_static(
                "geometry bounds must be finite with non-negative size",
            )));
        }

        let id = self.entries.insert_with(|_| GeometryRecord {
            desc: std::sync::Arc::new(desc),
        });
        self.peak = self.peak.max(self.entries.len());
        Ok(id)
    }

    /// 释放一份几何（代际 +1，旧句柄立即失效）。
    ///
    /// 返回 `false` = 句柄陈旧（已释放过或从未存在）；陈旧释放是**幂等无害**的，
    /// 应用在 `Drop` 里兜底释放时不需要判断。
    pub fn release(&mut self, id: GeometryId) -> bool {
        self.entries.remove(id).is_some()
    }

    /// 按句柄取记录（**同时**校验 index 与 generation）。
    #[cfg(test)]
    pub(crate) fn get(&self, id: GeometryId) -> Option<&GeometryRecord> {
        self.entries.get(id)
    }

    /// 按句柄取注册描述的共享句柄（`Arc` 克隆，不复制顶点字节）。
    ///
    /// store 用 `Arc<GeometryDesc>` 存描述，因此这里只是引用计数 +1；编码器把句柄附到
    /// `GeometryDrawGpu` 上，GPU 线程首次见到该 id 时据此建缓冲（见 `instance.rs`）。
    pub(crate) fn desc_arc(&self, id: GeometryId) -> Option<std::sync::Arc<GeometryDesc>> {
        self.entries
            .get(id)
            .map(|record| std::sync::Arc::clone(&record.desc))
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

/// 构造 [`EngineError::GeometryRegister`]（本模块所有校验失败共用）。
fn register_error(detail: SmolStr) -> EngineError {
    EngineError::GeometryRegister { detail }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::GeometryStore;
    use crate::{
        math::Rect,
        paint::{
            atlas::AtlasKind,
            geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology},
        },
    };

    /// 一个合法的 12 B/顶点 + u16 索引描述（单位三角形：3 顶点 × 12 B = 36 B）。
    ///
    /// `Ui` 的顶点布局是 `position: [f32; 2]` + 解析 AA 的 `coverage: f32`（R05-6）。
    fn triangle_desc() -> GeometryDesc {
        let mut vertex_bytes = [0u8; 36];
        // 三个顶点：(0,0)、(1,0)、(0,2)；覆盖率距离 -1（内部）。
        let points: [[f32; 3]; 3] = [[0.0, 0.0, -1.0], [1.0, 0.0, -1.0], [0.0, 2.0, -1.0]];
        for (slot, point) in vertex_bytes.as_chunks_mut::<12>().0.iter_mut().zip(points) {
            slot[..4].copy_from_slice(&point[0].to_le_bytes());
            slot[4..8].copy_from_slice(&point[1].to_le_bytes());
            slot[8..].copy_from_slice(&point[2].to_le_bytes());
        }
        GeometryDesc {
            vertex_bytes: Bytes::copy_from_slice(&vertex_bytes),
            // 3 个 u16 索引。
            index_bytes: Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: 12,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 1.0, 2.0),
            pipeline: GeometryPipeline::Ui,
        }
    }

    #[test]
    fn insert_returns_a_usable_handle() {
        let mut store = GeometryStore::new();
        assert!(store.is_empty());
        let id = store.insert(triangle_desc()).expect("valid desc");
        assert_eq!(store.len(), 1);
        let record = store.get(id).expect("registered");
        assert_eq!(record.desc.vertex_bytes.len(), 36);
        assert_eq!(record.desc.index_bytes.len(), 6);
        assert_eq!(record.desc.bounds, Rect::new(0.0, 0.0, 1.0, 2.0));
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
        assert!(store.get(id).is_none(), "释放后必须查不到");
        assert!(store.is_empty());
        assert!(!store.release(id), "重复释放是幂等的 false");
    }

    /// 槽位复用后代际 +1：旧句柄**不能**读到新几何（否则释放再注册会画错东西）。
    #[test]
    fn a_reused_slot_does_not_resurrect_the_old_handle() {
        let mut store = GeometryStore::new();
        let old = store.insert(triangle_desc()).expect("old");
        assert!(store.release(old));
        let new = store.insert(triangle_desc()).expect("new");
        assert_eq!(new.index(), old.index(), "槽位必须复用");
        assert_ne!(new.generation(), old.generation(), "代际必须递增");
        assert!(store.get(old).is_none(), "旧句柄必须失效");
        assert!(store.get(new).is_some());
    }

    #[test]
    fn stride_mismatch_is_rejected() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.vertex_stride = 16; // 与 `Ui` 的 8 B 不符
        assert!(store.insert(desc).is_err());
        assert!(store.is_empty(), "失败的注册不得留下槽位");
    }

    #[test]
    fn non_multiple_vertex_bytes_are_rejected() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.vertex_bytes = Bytes::from_static(&[0, 0, 0]);
        assert!(store.insert(desc).is_err());
    }

    #[test]
    fn empty_buffers_are_rejected() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.vertex_bytes = Bytes::new();
        assert!(store.insert(desc).is_err());

        let mut desc = triangle_desc();
        desc.index_bytes = Bytes::new();
        assert!(store.insert(desc).is_err());
    }

    #[test]
    fn non_multiple_index_bytes_are_rejected() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.index_bytes = Bytes::from_static(&[0, 0, 1]); // 3 B 不是 u16 的整数倍
        assert!(store.insert(desc).is_err());
    }

    #[test]
    fn non_finite_bounds_are_rejected() {
        let mut store = GeometryStore::new();
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

    /// R05-6：`Ui` 顶点必须带解析 AA 的覆盖率属性（12 B / 顶点）。
    ///
    /// 旧的 8 B 布局（只有 `position`）会让 `geometry.ui.*` 的顶点着色器读到垃圾覆盖率 ——
    /// 这里在注册期就拒绝，而不是让它悄悄画成半透明或全透明。
    #[test]
    fn ui_pipeline_rejects_the_legacy_8_byte_layout() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.vertex_stride = 8;
        desc.vertex_bytes = Bytes::from_static(&[0; 24]); // 3 × 8 B（旧布局）
        let error = store.insert(desc).expect_err("8 B 的 Ui 顶点必须被拒绝");
        let text = error.to_string();
        assert!(text.contains("vertex_stride"), "{text}");
        assert!(!store.len().eq(&1), "拒绝后不得占用槽位");
        // 12 B / 顶点（position + coverage）才是合法布局。
        let mut store = GeometryStore::new();
        let id = store.insert(triangle_desc()).expect("12 B 布局合法");
        assert_eq!(
            store.get(id).expect("record").desc.vertex_stride,
            GeometryPipeline::Ui.vertex_stride()
        );
    }

    #[test]
    fn textured_pipeline_uses_the_16_byte_stride() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.pipeline = GeometryPipeline::Textured {
            atlas: AtlasKind::Image,
        };
        desc.vertex_stride = 16;
        // 3 顶点 × 16 B。
        desc.vertex_bytes = Bytes::from_static(&[0; 48]);
        let id = store.insert(desc).expect("textured desc");
        assert_eq!(store.get(id).expect("record").desc.vertex_bytes.len(), 48);
    }

    #[test]
    fn u32_indices_are_counted_by_four_bytes() {
        let mut store = GeometryStore::new();
        let mut desc = triangle_desc();
        desc.index_type = IndexType::U32;
        desc.index_bytes = Bytes::from_static(&[0; 12]); // 3 个 u32
        let id = store.insert(desc).expect("u32 desc");
        assert_eq!(store.get(id).expect("record").desc.index_bytes.len(), 12);
    }

    #[test]
    fn failed_insert_does_not_move_the_peak() {
        let mut store = GeometryStore::new();
        let _ = store.insert(triangle_desc()).expect("valid");
        let mut bad = triangle_desc();
        bad.vertex_stride = 99;
        assert!(store.insert(bad).is_err());
        assert_eq!(store.peak(), 1);
        assert_eq!(store.len(), 1);
    }
}
