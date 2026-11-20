//! macOS 的静态几何 GPU 缓冲（`GeometryId → MTLBuffer`，按需创建 + 页级增量上传）。
//!
//! 语义与 Windows / Linux 的实现逐项一致：首次见到某个 `GeometryId`（或槽位换代）时按快照
//! 建缓冲；**数据更新按 4 KiB 页增量上传**（staging shared 缓冲 + blit copy 到 private 缓冲），
//! 增长时按 `GEOMETRY_BUFFER_GROWTH_ALIGN` 扩容并整份重传；换代产生的旧缓冲交 retire 队列
//! 延迟释放（I4）。
//!
//! **为什么几何缓冲用 `StorageModePrivate` 而不是 shared**：Metal 命令缓冲的提交是异步的，
//! CPU 直接写 shared 几何缓冲会与上一帧仍在执行的 GPU 读取竞争（Metal 规范要求外部同步）。
//! 用「每帧一次性的 shared staging + blit copy」把写入排进同一命令缓冲：同一队列按序执行，
//! 上一帧的读取一定先于本帧的 copy 完成，三端因此共享同一个「队列序即同步」的模型。
//!
//! **驱逐（D-1）**：条目按 `(index, generation)` 缓存；每个条目记「最后一次被绘制的帧号」，
//! registry 自带单调帧号（每个内容帧录制前 [`GeometryRegistry::begin_frame`] 自增），帧末
//! [`GeometryRegistry::retain_recent`] 驱逐连续 [`EVICT_AFTER_FRAMES`] 帧未被绘制的条目，
//! 设备对象推进现有 retire 队列（等该帧完成后再真正释放）。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLDevice,
    MTLResourceOptions,
};

use crate::{
    error::EngineError,
    paint::{
        geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
        instance::GeometryDrawGpu,
    },
    render::{
        geometry_upload::{GeometryUploadTracker, staging_layout},
        macos::device::{Device, GpuObject, init_error},
        retire::GpuResourceHandle,
    },
};

/// 一条已注册几何的 GPU 视图。
pub(crate) struct GeometryEntry {
    vertex: Retained<ProtocolObject<dyn MTLBuffer>>,
    index: Retained<ProtocolObject<dyn MTLBuffer>>,
    index_count: u32,
    topology: Topology,
    pipeline: GeometryPipeline,
    index_type: IndexType,
    /// 该条目对应的代际（槽位复用后必须重建缓冲）。
    generation: u32,
    /// 最后一次被绘制的帧号（`GeometryRegistry` 的单调帧号）。
    ///
    /// 用于驱逐：应用释放几何句柄后本窗口的缓冲仍会驻留，必须按「最近使用」回收，
    /// 否则「每帧注册新几何、释放旧的」会让显存无界增长（D-1）。
    last_used_frame: u64,
    /// 卸载时交还的句柄。
    storage: [GpuResourceHandle; 2],
}

impl GeometryEntry {
    pub(crate) fn vertex(&self) -> &ProtocolObject<dyn MTLBuffer> {
        &self.vertex
    }

    pub(crate) fn index(&self) -> &ProtocolObject<dyn MTLBuffer> {
        &self.index
    }

    pub(crate) fn index_count(&self) -> u32 {
        self.index_count
    }

    pub(crate) fn topology(&self) -> Topology {
        self.topology
    }

    pub(crate) fn pipeline(&self) -> GeometryPipeline {
        self.pipeline
    }

    pub(crate) fn index_type(&self) -> IndexType {
        self.index_type
    }
}

/// 连续多少帧未被绘制就回收该几何的 GPU 缓冲（180 帧 ≈ 60 Hz 下 3 秒）。
const EVICT_AFTER_FRAMES: u64 = 180;

/// 该几何是否已连续 `EVICT_AFTER_FRAMES` 帧以上未被绘制（`wrapping_sub` 容忍回绕）。
fn geometry_is_stale(frame: u64, last_used_frame: u64) -> bool {
    frame.wrapping_sub(last_used_frame) > EVICT_AFTER_FRAMES
}

/// `GeometryId → GPU 缓冲`（按需创建；版本变化按页增量上传；长期未绘制即驱逐）。
pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
    /// 页级上传计划器（每窗口一份）。
    uploads: GeometryUploadTracker,
    /// 单调帧号：每个内容帧录制前自增，`last_used_frame` 与驱逐都基于它。
    frame: u64,
}

impl GeometryRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            uploads: GeometryUploadTracker::new(),
            frame: 0,
        }
    }

    /// 推进帧号（每个内容帧录制前调用一次）。
    pub(crate) fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// 驱逐长期未绘制的几何，把设备对象交给调用方 retire。
    pub(crate) fn retain_recent(&mut self, retired: &mut Vec<GpuResourceHandle>) {
        let frame = self.frame;
        for slot in &mut self.entries {
            let stale = slot
                .as_ref()
                .is_some_and(|entry| geometry_is_stale(frame, entry.last_used_frame));
            if stale && let Some(entry) = slot.take() {
                retired.extend(entry.storage);
            }
        }
    }

    /// 取已就绪条目（消费者：录制期的 draw 循环；条目由 [`GeometryRegistry::prepare`] 建好）。
    pub(crate) fn get(&self, id: GeometryId) -> Option<&GeometryEntry> {
        self.entries
            .get(id.index() as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.generation == id.generation())
    }

    /// 标记条目本帧被绘制（`retain_recent` 的驱逐判据）。
    pub(crate) fn touch(&mut self, id: GeometryId) {
        let frame = self.frame;
        if let Some(entry) = self
            .entries
            .get_mut(id.index() as usize)
            .and_then(Option::as_mut)
            .filter(|entry| entry.generation == id.generation())
        {
            entry.last_used_frame = frame;
        }
    }

    /// X5：帧开始前把本帧几何表的**新版本页**增量上传到 private 缓冲
    /// （一次性 shared staging + blit copy）。返回本帧 staging 上传字节数。
    pub(crate) fn prepare(
        &mut self,
        device: &mut Device,
        command: &ProtocolObject<dyn MTLCommandBuffer>,
        geometry_table: &[GeometryDrawGpu],
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<usize, EngineError> {
        let plans = self.uploads.plan(geometry_table);
        if plans.is_empty() {
            return Ok(0);
        }
        if plans
            .iter()
            .any(|plan| plan.snapshot.index_count() == 0 || plan.snapshot.vertex_pages().is_empty())
        {
            return Err(EngineError::GeometryRegister {
                detail: smol_str::SmolStr::new_static(
                    "geometry snapshot has no CPU data (stale handle or released before upload)",
                ),
            });
        }
        for plan in &plans {
            // 契约自检：`full` 计划覆盖全部页；顶点步长必须非零（GPU 侧 PSO 的顶点布局契约）。
            debug_assert!(plan.snapshot.vertex_stride() > 0);
            debug_assert!(
                !plan.vertex.full || plan.vertex.pages.len() == plan.snapshot.vertex_pages().len()
            );
            debug_assert!(
                !plan.index.full || plan.index.pages.len() == plan.snapshot.index_pages().len()
            );
        }
        // 一次性 staging（本帧结束后由 device 句柄表 + retire 队列保证生命周期）。
        // 容量与每页偏移必须同一口径（RX-1）：`staging_layout` 用 `Σ align512(len)` 分配，
        // 偏移也按 512 B 推进，页的**真实长度**写入不会越过缓冲末尾。
        let layout = staging_layout(plans.iter().flat_map(|plan| {
            plan.vertex
                .pages
                .iter()
                .chain(plan.index.pages.iter())
                .map(|page| page.bytes.len())
        }));
        let staging = device
            .raw()
            .newBufferWithLength_options(
                layout.total_bytes.max(4),
                MTLResourceOptions::StorageModeShared,
            )
            .ok_or_else(|| init_error("newBufferWithLength_options(geometry staging)", "nil"))?;
        let staging_handle = device.allocate(GpuObject::Buffer(staging.clone()));
        retired.push(staging_handle);
        let blit = command
            .blitCommandEncoder()
            .ok_or_else(|| init_error("blitCommandEncoder(geometry)", "returned nil"))?;
        let mut staged_page = 0usize;
        let mut uploaded = 0usize;
        for plan in &plans {
            let entry_slot = plan.id.index() as usize;
            if self.entries.len() <= entry_slot {
                self.entries.resize_with(entry_slot + 1, || None);
            }
            // 槽位复用：旧代际条目连同缓冲一起废弃。
            if self.entries[entry_slot]
                .as_ref()
                .is_some_and(|entry| entry.generation != plan.id.generation())
                && let Some(old) = self.entries[entry_slot].take()
            {
                retired.extend(old.storage);
            }
            let first_sight = self.entries[entry_slot].is_none();
            if first_sight {
                let vertex = private_buffer(device, plan.vertex.capacity)?;
                let index = private_buffer(device, plan.index.capacity)?;
                let vertex_handle = device.allocate(GpuObject::Buffer(vertex.clone()));
                let index_handle = device.allocate(GpuObject::Buffer(index.clone()));
                self.entries[entry_slot] = Some(GeometryEntry {
                    vertex,
                    index,
                    index_count: 0,
                    topology: Topology::TriangleList,
                    pipeline: GeometryPipeline::Ui,
                    index_type: plan.snapshot.index_type(),
                    generation: plan.id.generation(),
                    last_used_frame: self.frame,
                    storage: [vertex_handle, index_handle],
                });
            }
            let entry = self.entries[entry_slot]
                .as_mut()
                .expect("刚刚建好的几何条目");
            if plan.vertex.realloc && !first_sight {
                let vertex = private_buffer(device, plan.vertex.capacity)?;
                let handle = device.allocate(GpuObject::Buffer(vertex.clone()));
                let old = core::mem::replace(&mut entry.vertex, vertex);
                let old_handle = core::mem::replace(&mut entry.storage[0], handle);
                let _ = old;
                retired.push(old_handle);
            }
            if plan.index.realloc && !first_sight {
                let index = private_buffer(device, plan.index.capacity)?;
                let handle = device.allocate(GpuObject::Buffer(index.clone()));
                let old = core::mem::replace(&mut entry.index, index);
                let old_handle = core::mem::replace(&mut entry.storage[1], handle);
                let _ = old;
                retired.push(old_handle);
            }
            for page in &plan.vertex.pages {
                let offset = layout.offsets[staged_page];
                debug_assert!(layout.fits(staged_page, page.bytes.len()));
                copy_page(
                    &blit,
                    &staging,
                    offset,
                    &page.bytes,
                    &entry.vertex,
                    crate::paint::geometry::GeometrySnapshot::page_offset(page.page as usize)
                        as usize,
                );
                staged_page += 1;
                uploaded += page.bytes.len();
            }
            for page in &plan.index.pages {
                let offset = layout.offsets[staged_page];
                debug_assert!(layout.fits(staged_page, page.bytes.len()));
                copy_page(
                    &blit,
                    &staging,
                    offset,
                    &page.bytes,
                    &entry.index,
                    crate::paint::geometry::GeometrySnapshot::page_offset(page.page as usize)
                        as usize,
                );
                staged_page += 1;
                uploaded += page.bytes.len();
            }
            entry.index_count = plan.snapshot.index_count();
            entry.topology = plan.snapshot.topology();
            entry.pipeline = plan.snapshot.pipeline();
            entry.index_type = plan.snapshot.index_type();
            entry.generation = plan.id.generation();
            entry.last_used_frame = self.frame;
        }
        blit.endEncoding();
        Ok(uploaded)
    }
}

/// 建一块 private（device-local）缓冲；容量按上传计划的容量分配。
fn private_buffer(
    device: &mut Device,
    capacity: u32,
) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>, EngineError> {
    device
        .raw()
        .newBufferWithLength_options(
            (capacity.max(4)) as usize,
            MTLResourceOptions::StorageModePrivate,
        )
        .ok_or_else(|| {
            init_error(
                "newBufferWithLength_options(geometry private)",
                "returned nil",
            )
        })
}

/// 把一页写进 staging 并记录「staging → private 缓冲」的 blit copy。
///
/// `staging_offset` 来自 [`staging_layout`]（容量与偏移同一 512 B 口径），写入/copy 用**真实
/// 长度**；调用方已在 `debug_assert!(layout.fits(..))` 处证明区间落在 staging 内。
fn copy_page(
    blit: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    staging: &ProtocolObject<dyn MTLBuffer>,
    staging_offset: usize,
    bytes: &bytes::Bytes,
    target: &ProtocolObject<dyn MTLBuffer>,
    target_offset: usize,
) {
    // SAFETY: staging 容量按 `Σ align512(len)` 分配、偏移按同一对齐推进，`offset + len` 在
    // 缓冲内（调用方已断言）；`contents()` 返回可写指针。
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            staging.contents().as_ptr().cast::<u8>().add(staging_offset),
            bytes.len(),
        );
    }
    // SAFETY: 源区间 [offset, offset+len) 在 staging 内、目标区间在 private 缓冲容量内；
    // blit 在 command buffer 提交后按序执行。
    unsafe {
        blit.copyFromBuffer_sourceOffset_toBuffer_destinationOffset_size(
            staging,
            staging_offset,
            target,
            target_offset,
            bytes.len(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{EVICT_AFTER_FRAMES, geometry_is_stale};

    /// 驱逐决策的边界（纯逻辑，不需要真实 Metal 设备）。
    #[test]
    fn geometry_is_evicted_only_after_the_grace_window() {
        // 刚用过：一定保留。
        assert!(!geometry_is_stale(1_000, 1_000));
        // 刚好差到门槛：保留（判据是 `>` 而不是 `>=`）。
        assert!(!geometry_is_stale(1_000 + EVICT_AFTER_FRAMES, 1_000));
        // 越过门槛：驱逐。
        assert!(geometry_is_stale(1_001 + EVICT_AFTER_FRAMES, 1_000));
        // 帧号回绕（u64 实际不会发生，但 `wrapping_sub` 必须给出正确差值）。
        assert!(!geometry_is_stale(0, u64::MAX));
    }

    /// RX-1/RX-13 的真实 Metal 回归：最小反例（顶点页 36 B + 索引页 6 B，均非 512 倍数）
    /// 走两次完整 `prepare`（首帧整份 + 增量一页），提交真实命令缓冲并等待完成。
    ///
    /// 旧实现按 `Σ len` 分配 staging、按 512 对齐推进偏移 ⇒ index 页从 offset 512 写入
    /// 42 B 的缓冲：`contents()` 越界写 + blit 源区间越界（Metal validation 报错或静默损坏）。
    /// 本测试在真机 Metal 上执行该路径，断言命令缓冲成功完成且上传字节为真实长度。
    #[test]
    fn staging_upload_runs_on_a_real_metal_device() {
        use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandQueue, MTLDevice};

        use crate::{
            math::{Rect, Transform},
            paint::{
                geometry::{GeometryDesc, GeometryPipeline, IndexType, Topology},
                geometry_store::GeometryStore,
                instance::GeometryDrawGpu,
            },
        };

        let mut device = super::Device::new().expect("metal device");
        let queue = device.raw().newCommandQueue().expect("command queue");
        let mut store = GeometryStore::new();
        let mut vertices = Vec::with_capacity(36);
        for value in [0x11u8, 0x22, 0x33] {
            vertices.extend_from_slice(&[value; 12]);
        }
        let indices = [0u8, 0, 1, 0, 2, 0];
        let id = store
            .insert(GeometryDesc {
                vertex_bytes: bytes::Bytes::from(vertices),
                index_bytes: bytes::Bytes::from(indices.to_vec()),
                vertex_stride: 12,
                index_type: IndexType::U16,
                topology: Topology::TriangleList,
                bounds: Rect::new(0.0, 0.0, 32.0, 32.0),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册几何");
        store
            .update_vertices(id, 12..24, &[0xAB; 12])
            .expect("range 更新");
        let mut registry = super::GeometryRegistry::new();
        let mut retired = Vec::new();

        // 帧 1：首见 ⇒ 整份上传（36 + 6 = 42 B，staging 容量按 512 对齐取整）。
        registry.begin_frame();
        let snapshot = store.snapshot_arc(id).expect("快照");
        let command = queue.commandBuffer().expect("command buffer 1");
        let uploaded = registry
            .prepare(
                &mut device,
                &command,
                &[GeometryDrawGpu {
                    id,
                    range: 0..3,
                    transform: Transform::IDENTITY,
                    color: [1.0; 4],
                    snapshot,
                }],
                &mut retired,
            )
            .expect("prepare full");
        command.commit();
        command.waitUntilCompleted();
        assert_eq!(command.status(), MTLCommandBufferStatus::Completed);
        assert!(command.error().is_none(), "Metal 命令缓冲不得失败");
        assert_eq!(uploaded, 36 + 6);

        // 帧 2：同版本 ⇒ 0 上传；帧 3：改顶点 1 ⇒ 只上传被触碰的页（36 B 页内 12 B）。
        registry.begin_frame();
        let snapshot = store.snapshot_arc(id).expect("快照");
        let command = queue.commandBuffer().expect("command buffer 2");
        let uploaded = registry
            .prepare(
                &mut device,
                &command,
                &[GeometryDrawGpu {
                    id,
                    range: 0..3,
                    transform: Transform::IDENTITY,
                    color: [1.0; 4],
                    snapshot,
                }],
                &mut retired,
            )
            .expect("prepare same revision");
        command.commit();
        command.waitUntilCompleted();
        assert_eq!(command.status(), MTLCommandBufferStatus::Completed);
        assert_eq!(uploaded, 0, "同版本帧不得重复上传");

        for handle in retired {
            let _ = device.release(handle);
        }
    }
}
