//! Linux 的静态几何 GPU 缓冲（`GeometryId → 顶点/索引缓冲`，按需创建 + 页级增量上传）。
//!
//! 语义与 Windows / macOS 的实现逐项一致：首次见到某个 `GeometryId`（或槽位换代）时按快照
//! 建缓冲；**数据更新按 4 KiB 页增量上传**（一次性 staging 缓冲 + `vkCmdCopyBuffer`），增长时
//! 按 `GEOMETRY_BUFFER_GROWTH_ALIGN` 扩容并整份重传；换代产生的旧缓冲交 retire 队列延迟释放
//! （I4）。
//!
//! **为什么副本走 staging + `vkCmdCopyBuffer` 而不是直接写 host-visible 几何缓冲**：Vulkan 的
//! host 写入与 GPU 读取之间没有隐式同步，直接改映射内存会与在飞帧竞争。把写入排进同一队列的
//! 命令缓冲后，上一帧的读取一定先于本帧的 copy 完成 —— 三端共享同一个「队列序即同步」模型。
//!
//! **内存选择**：几何缓冲仍用 [`MemoryNeed::HOST_WRITABLE`]（BAR / UMA 上落在设备本地，与
//! Windows `DEFAULT` / Metal `.private` 等价；独显无 BAR 时退回主机堆）：创建期整份写入后，
//! 运行期不再 CPU 写它（只走 staging copy）。
//!
//! **驱逐（D-1）**：条目按 `(index, generation)` 缓存；每个条目记「最后一次被绘制的帧号」，
//! registry 自带单调帧号（每个内容帧录制前 [`GeometryRegistry::begin_frame`] 自增），帧末
//! [`GeometryRegistry::retain_recent`] 驱逐连续 [`EVICT_AFTER_FRAMES`] 帧未被绘制的条目，
//! 设备对象推进现有 retire 队列（等该帧完成后再真正释放）。

use ash::vk;

use crate::{
    error::EngineError,
    paint::{
        geometry::{GeometryId, GeometryPipeline, IndexType, Topology},
        instance::GeometryDrawGpu,
    },
    render::{
        geometry_upload::{GeometryUploadTracker, staging_layout},
        linux::{
            device::{GpuBuffer, SharedGpu, init_error},
            memory::MemoryNeed,
        },
        retire::GpuResourceHandle,
    },
};

/// 连续多少帧未被绘制就回收该几何的 GPU 缓冲。
///
/// 180 帧 ≈ 60 Hz 下的 3 秒：足够覆盖「隔帧 / 低频动画」的几何，同时让被应用释放的静态
/// 几何在几秒内归还显存（Chrome 的 discardable GPU cache 用同类按使用时间的策略）。
const EVICT_AFTER_FRAMES: u64 = 180;

/// 该几何是否已连续 `EVICT_AFTER_FRAMES` 帧以上未被绘制（`wrapping_sub` 容忍回绕）。
fn geometry_is_stale(frame: u64, last_used_frame: u64) -> bool {
    frame.wrapping_sub(last_used_frame) > EVICT_AFTER_FRAMES
}

/// 一条已注册几何的 GPU 视图。
pub(crate) struct GeometryEntry {
    /// 顶点缓冲（子分配：`buffer` + 块内区间 + 主机写入指针 + retire 句柄）。
    vertex: GpuBuffer,
    index: GpuBuffer,
    index_count: u32,
    /// 图元类型（Vulkan 把它烘进管线状态：PSO 按它选，绘制时不需要单独绑定）。
    pipeline: GeometryPipeline,
    /// 三角拓扑（Vulkan 的 `VkPipelineInputAssemblyStateCreateInfo::topology` 是 PSO 烘死状态，
    /// 不能逐 draw 切换 ⇒ 绘制时必须选对应 PSO，与 D3D12 / Metal 同语义）。
    topology: Topology,
    /// 索引格式（`cmd_bind_index_buffer` 需要）。
    index_type: vk::IndexType,
    /// 该条目对应的代际（槽位复用后必须重建缓冲）。
    generation: u32,
    /// 最后一次被绘制的帧号（[`GeometryRegistry`] 的单调帧号；驱逐判据）。
    last_used_frame: u64,
}

impl GeometryEntry {
    pub(crate) fn vertex(&self) -> vk::Buffer {
        self.vertex.buffer
    }

    pub(crate) fn index(&self) -> vk::Buffer {
        self.index.buffer
    }

    pub(crate) fn index_count(&self) -> u32 {
        self.index_count
    }

    pub(crate) fn pipeline(&self) -> GeometryPipeline {
        self.pipeline
    }

    /// 三角拓扑（PSO 选择的一部分）。
    pub(crate) fn topology(&self) -> Topology {
        self.topology
    }

    /// 索引格式。
    pub(crate) fn index_type(&self) -> vk::IndexType {
        self.index_type
    }

    /// 卸载时交还共享设备的句柄（顶点 + 索引；缓冲销毁与区间归还都在 `SharedGpu::release`）。
    fn handles(&self) -> [GpuResourceHandle; 2] {
        [self.vertex.handle, self.index.handle]
    }
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

    /// 驱逐长期未绘制的几何，把设备对象句柄交给调用方 retire。
    pub(crate) fn retain_recent(&mut self, retired: &mut Vec<GpuResourceHandle>) {
        let frame = self.frame;
        for slot in &mut self.entries {
            let stale = slot
                .as_ref()
                .is_some_and(|entry| geometry_is_stale(frame, entry.last_used_frame));
            if stale && let Some(entry) = slot.take() {
                retired.extend(entry.handles());
            }
        }
    }

    /// teardown：把本窗口几何缓冲的句柄交回共享设备。
    pub(crate) fn release(&self, shared: &SharedGpu) {
        for entry in self.entries.iter().flatten() {
            for handle in entry.handles() {
                let _ = shared.release(handle);
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

    /// X5：把本帧几何表的**新版本页**增量上传到几何缓冲
    /// （一次性 staging 缓冲 + `vkCmdCopyBuffer`）。返回本帧 staging 上传字节数。
    ///
    /// 必须在 **begin render pass 之前**调用（copy 与 buffer barrier 不能出现在 render pass 内）。
    pub(crate) fn prepare(
        &mut self,
        shared: &SharedGpu,
        command: vk::CommandBuffer,
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
            debug_assert!(plan.snapshot.vertex_stride() > 0);
            debug_assert!(
                !plan.vertex.full || plan.vertex.pages.len() == plan.snapshot.vertex_pages().len()
            );
            debug_assert!(
                !plan.index.full || plan.index.pages.len() == plan.snapshot.index_pages().len()
            );
        }
        // 容量与每页偏移必须同一口径（RX-1）：`staging_layout` 用 `Σ align512(len)` 分配，
        // 偏移也按 512 B 推进，页的**真实长度**写入不会越过缓冲末尾。
        let layout = staging_layout(plans.iter().flat_map(|plan| {
            plan.vertex
                .pages
                .iter()
                .chain(plan.index.pages.iter())
                .map(|page| page.bytes.len())
        }));
        let staging = staging_buffer(shared, layout.total_bytes.max(4) as u64)?;
        let Some(staging_pointer) = staging.pointer else {
            let _ = shared.release(staging.handle);
            return Err(init_error(
                "geometry staging",
                "suballocated staging buffer has no host mapping",
            ));
        };
        let staging_handle = staging.handle;
        let staging_buffer = staging.buffer;
        let mut staged_page = 0usize;
        let mut uploaded = 0usize;
        for plan in &plans {
            let entry_slot = plan.id.index() as usize;
            if self.entries.len() <= entry_slot {
                self.entries.resize_with(entry_slot + 1, || None);
            }
            if self.entries[entry_slot]
                .as_ref()
                .is_some_and(|entry| entry.generation != plan.id.generation())
                && let Some(old) = self.entries[entry_slot].take()
            {
                retired.extend(old.handles());
            }
            let first_sight = self.entries[entry_slot].is_none();
            if first_sight {
                let vertex = create_geometry_buffer(shared, plan.vertex.capacity)?;
                let index = create_geometry_buffer(shared, plan.index.capacity)?;
                self.entries[entry_slot] = Some(GeometryEntry {
                    vertex,
                    index,
                    index_count: 0,
                    pipeline: GeometryPipeline::Ui,
                    topology: Topology::TriangleList,
                    index_type: index_type(plan.snapshot.index_type()),
                    generation: plan.id.generation(),
                    last_used_frame: self.frame,
                });
            }
            let entry = self.entries[entry_slot]
                .as_mut()
                .expect("刚刚建好的几何条目");
            if plan.vertex.realloc && !first_sight {
                let vertex = create_geometry_buffer(shared, plan.vertex.capacity)?;
                let old = core::mem::replace(&mut entry.vertex, vertex);
                retired.push(old.handle);
            }
            if plan.index.realloc && !first_sight {
                let index = create_geometry_buffer(shared, plan.index.capacity)?;
                let old = core::mem::replace(&mut entry.index, index);
                retired.push(old.handle);
            }
            // 逐页拷进 staging；同一目标的 copy 合并成一次 `cmd_copy_buffer`（一个
            // barrier 对覆盖整组页，避免每页一次 pipeline barrier）。
            let mut vertex_copies: Vec<vk::BufferCopy> = Vec::new();
            let mut index_copies: Vec<vk::BufferCopy> = Vec::new();
            for page in &plan.vertex.pages {
                let src_offset = layout.offsets[staged_page];
                debug_assert!(layout.fits(staged_page, page.bytes.len()));
                stage_page(&staging_pointer, src_offset, &page.bytes);
                staged_page += 1;
                vertex_copies.push(
                    vk::BufferCopy::default()
                        .src_offset(src_offset as u64)
                        .dst_offset(crate::paint::geometry::GeometrySnapshot::page_offset(
                            page.page as usize,
                        ) as u64)
                        .size(page.bytes.len() as u64),
                );
                uploaded += page.bytes.len();
            }
            for page in &plan.index.pages {
                let src_offset = layout.offsets[staged_page];
                debug_assert!(layout.fits(staged_page, page.bytes.len()));
                stage_page(&staging_pointer, src_offset, &page.bytes);
                staged_page += 1;
                index_copies.push(
                    vk::BufferCopy::default()
                        .src_offset(src_offset as u64)
                        .dst_offset(crate::paint::geometry::GeometrySnapshot::page_offset(
                            page.page as usize,
                        ) as u64)
                        .size(page.bytes.len() as u64),
                );
                uploaded += page.bytes.len();
            }
            if !vertex_copies.is_empty() {
                buffers_to_copy(
                    shared,
                    command,
                    &vertex_copies,
                    entry.vertex.buffer,
                    staging_buffer,
                );
            }
            if !index_copies.is_empty() {
                buffers_to_copy(
                    shared,
                    command,
                    &index_copies,
                    entry.index.buffer,
                    staging_buffer,
                );
            }
            entry.index_count = plan.snapshot.index_count();
            entry.pipeline = plan.snapshot.pipeline();
            entry.topology = plan.snapshot.topology();
            entry.index_type = index_type(plan.snapshot.index_type());
            entry.generation = plan.id.generation();
            entry.last_used_frame = self.frame;
        }
        retired.push(staging_handle);
        Ok(uploaded)
    }
}

/// 建一块 host 可写几何缓冲（`VERTEX|INDEX|TRANSFER_DST`；BAR/UMA 上优先设备本地）。
fn create_geometry_buffer(shared: &SharedGpu, capacity: u32) -> Result<GpuBuffer, EngineError> {
    shared.allocate_buffer(
        u64::from(capacity.max(4)),
        vk::BufferUsageFlags::VERTEX_BUFFER
            | vk::BufferUsageFlags::INDEX_BUFFER
            | vk::BufferUsageFlags::TRANSFER_DST,
        MemoryNeed::HOST_WRITABLE,
    )
}

/// 建一块 staging 缓冲（`TRANSFER_SRC` + host visible）。
fn staging_buffer(shared: &SharedGpu, bytes: u64) -> Result<GpuBuffer, EngineError> {
    shared.allocate_buffer(
        bytes.max(4),
        vk::BufferUsageFlags::TRANSFER_SRC,
        MemoryNeed::HOST_WRITABLE,
    )
}

/// 把一页写进 staging 的 `offset`（来自 [`staging_layout`]，容量与偏移同一 512 B 口径）。
///
/// 调用方已在 `debug_assert!(layout.fits(..))` 处证明 `offset + len` 落在 staging 内；
/// 写入用**真实长度**（不是对齐后的长度）。
fn stage_page(pointer: &*mut u8, offset: usize, bytes: &[u8]) {
    // SAFETY: staging 容量按 `Σ align512(len)` 分配、偏移按同一对齐推进，区间在缓冲内。
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.add(offset), bytes.len());
    }
}

/// 记录一次 buffer → buffer copy，并在 copy 前后做 `TRANSFER_WRITE` / `SHADER_READ` 的
/// pipeline barrier（同时覆盖上一帧的读取 → 本帧写入的依赖）。
fn buffers_to_copy(
    shared: &SharedGpu,
    command: vk::CommandBuffer,
    copies: &[vk::BufferCopy],
    destination: vk::Buffer,
    source: vk::Buffer,
) {
    let barrier_in = vk::BufferMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::SHADER_READ)
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(destination)
        .offset(0)
        .size(vk::WHOLE_SIZE);
    // SAFETY: 命令缓冲在录制状态；barrier/copy 的句柄都来自共享设备且本帧存活。
    unsafe {
        shared.raw().cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::VERTEX_SHADER | vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&barrier_in),
            &[],
        );
        shared
            .raw()
            .cmd_copy_buffer(command, source, destination, copies);
    }
    // copy 完成后再一次性过渡回 shader 读取：调用方在本函数返回后继续记录下一段的 copy，
    // 下一段自己的 barrier_in 会把「上一段的 shader 读取等待」覆盖掉 —— 这里用
    // `TRANSFER_WRITE → SHADER_READ` 的写后读依赖保证本段数据对后续 draw 可见。
    let barrier_out = vk::BufferMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::SHADER_READ)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(destination)
        .offset(0)
        .size(vk::WHOLE_SIZE);
    // SAFETY: 同上。
    unsafe {
        shared.raw().cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::VERTEX_SHADER | vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::DependencyFlags::empty(),
            &[],
            std::slice::from_ref(&barrier_out),
            &[],
        );
    }
}

/// 索引格式（`vk::IndexType`）。
fn index_type(kind: IndexType) -> vk::IndexType {
    match kind {
        IndexType::U16 => vk::IndexType::UINT16,
        IndexType::U32 => vk::IndexType::UINT32,
    }
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::{EVICT_AFTER_FRAMES, GeometryRegistry, geometry_is_stale};
    use crate::render::linux::device::SharedGpu;

    /// 驱逐判据的边界：恰好到门槛保留、越过门槛驱逐、帧号回绕给出正确差值。
    #[test]
    fn eviction_boundary_is_exclusive_and_wraps() {
        // 刚用过：一定保留。
        assert!(!geometry_is_stale(1_000, 1_000));
        // 刚好差到门槛：保留（判据是 `>` 而不是 `>=`）。
        assert!(!geometry_is_stale(1_000 + EVICT_AFTER_FRAMES, 1_000));
        // 越过门槛：驱逐。
        assert!(geometry_is_stale(1_001 + EVICT_AFTER_FRAMES, 1_000));
        // 帧号回绕（u64 实际不会发生，但 `wrapping_sub` 必须给出正确差值）。
        assert!(!geometry_is_stale(0, u64::MAX));
    }

    /// `begin_frame` 单调推进帧号（`touch` 的 `last_used_frame` 与驱逐都基于它）。
    #[test]
    fn begin_frame_advances_the_registry_clock() {
        let mut registry = GeometryRegistry::new();
        assert_eq!(registry.frame, 0);
        registry.begin_frame();
        registry.begin_frame();
        assert_eq!(registry.frame, 2);
    }

    /// 没有几何绘制的帧不产生任何计划/命令（`prepare` 早退，不需要可用的命令缓冲）。
    #[test]
    fn prepare_without_geometry_is_a_noop() {
        let shared = crate::render::linux::device::SharedGpu::new_headless_isolated()
            .expect("headless shared gpu");
        let mut registry = GeometryRegistry::new();
        registry.begin_frame();
        let mut retired = Vec::new();
        let before = shared.memory_stats();
        let uploaded = registry
            .prepare(&shared, vk::CommandBuffer::null(), &[], &mut retired)
            .expect("noop prepare");
        assert_eq!(uploaded, 0);
        assert!(retired.is_empty());
        assert_eq!(shared.memory_stats(), before, "无几何的帧不得分配 GPU 内存");
    }

    fn command_pool(gpu: &SharedGpu) -> vk::CommandPool {
        // SAFETY: 池归测试独占；结束时销毁。
        unsafe {
            gpu.raw()
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(gpu.queue_family())
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .expect("command pool")
        }
    }

    fn allocate(gpu: &SharedGpu, pool: vk::CommandPool) -> vk::CommandBuffer {
        // SAFETY: 池由调用方持有；只分配一条一次性命令缓冲。
        unsafe {
            gpu.raw()
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .expect("command buffer")[0]
        }
    }

    fn begin(gpu: &SharedGpu, command: vk::CommandBuffer) {
        // SAFETY: 命令缓冲由本测试独占；提交前只录制一次。
        unsafe {
            gpu.raw()
                .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
                .expect("reset command buffer");
            gpu.raw()
                .begin_command_buffer(
                    command,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )
                .expect("begin command buffer");
        }
    }

    fn submit_and_wait(gpu: &SharedGpu, command: vk::CommandBuffer) {
        // SAFETY: 命令缓冲已录制完毕；同步等待只发生在测试里。
        unsafe { gpu.raw().end_command_buffer(command) }.expect("end command buffer");
        let commands = [command];
        gpu.with_queue(|queue| unsafe {
            gpu.raw()
                .queue_submit(
                    queue,
                    &[vk::SubmitInfo::default().command_buffers(&commands)],
                    vk::Fence::null(),
                )
                .expect("queue submit");
        });
        gpu.wait_queue_idle().expect("queue idle");
    }

    /// RX-1/RX-13 的真实设备回归：最小反例（顶点页 36 B + 索引页 6 B，均非 512 倍数）
    /// 走一次完整 `prepare`（staging + `vkCmdCopyBuffer`），提交并等待后**读回**几何缓冲：
    ///
    /// - staging 越界（旧实现按 `Σ len` 分配、按 512 对齐推进）在 lavapipe/validation 下会
    ///   报 VUID 或读回内容错位；
    /// - 读回同时验证「增量的那一页」真的被 copy 到页偏移（不只是账本数字）。
    #[test]
    fn staging_upload_copies_real_page_lengths_on_a_real_device() {
        use crate::{
            math::{Rect, Transform},
            paint::{
                geometry::{
                    GEOMETRY_PAGE_BYTES, GeometryDesc, GeometryPipeline, IndexType, Topology,
                },
                geometry_store::GeometryStore,
                instance::GeometryDrawGpu,
            },
        };
        let shared = SharedGpu::new_headless_isolated().expect("headless shared gpu");
        let pool = command_pool(&shared);
        let mut store = GeometryStore::new();
        // 最小反例：3 顶点（12 B/顶点 = 36 B）+ 3 个 U16 索引（6 B）。
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
        // 增量更新中间那个顶点（offset 12..24）——只替换一页。
        store
            .update_vertices(id, 12..24, &[0xAB; 12])
            .expect("range 更新");
        let snapshot = store.snapshot_arc(id).expect("快照");
        let snapshot_vertex_len = snapshot.vertex_len();
        let draw = GeometryDrawGpu {
            id,
            chunk: 0,
            transform: Transform::IDENTITY,
            color: [1.0; 4],
            snapshot,
        };
        let mut registry = GeometryRegistry::new();
        registry.begin_frame();
        let mut retired = Vec::new();
        let command = allocate(&shared, pool);
        begin(&shared, command);
        let uploaded = registry
            .prepare(&shared, command, &[draw], &mut retired)
            .expect("prepare");
        submit_and_wait(&shared, command);
        assert_eq!(uploaded, 36 + 6, "首帧整份上传（真实长度，不含对齐填充）");
        let entry = registry.get(id).expect("几何条目");
        let vertex_pointer = entry.vertex.pointer.expect("几何缓冲必须可读回");
        // SAFETY: 队列已 idle，映射区间覆盖整个子分配块；读取 36 B。
        let vertex_bytes = unsafe { std::slice::from_raw_parts(vertex_pointer, 36) };
        assert_eq!(&vertex_bytes[0..12], &[0x11; 12]);
        assert_eq!(&vertex_bytes[12..24], &[0xAB; 12], "增量页必须落到页内偏移");
        assert_eq!(&vertex_bytes[24..36], &[0x33; 12]);
        let index_pointer = entry.index.pointer.expect("索引缓冲必须可读回");
        // SAFETY: 同上，读取 6 B。
        let index_bytes = unsafe { std::slice::from_raw_parts(index_pointer, 6) };
        assert_eq!(index_bytes, &[0u8, 0, 1, 0, 2, 0]);
        assert!(
            snapshot_vertex_len as usize <= GEOMETRY_PAGE_BYTES,
            "最小反例应落在单页内（本测试的目标是页内偏移而不是跨页）"
        );

        // 同版本再 prepare：零上传（tracker 幂等）。
        let snapshot = store.snapshot_arc(id).expect("快照");
        let draw = GeometryDrawGpu {
            id,
            chunk: 0,
            transform: Transform::IDENTITY,
            color: [1.0; 4],
            snapshot,
        };
        registry.begin_frame();
        let command = allocate(&shared, pool);
        begin(&shared, command);
        let uploaded = registry
            .prepare(&shared, command, &[draw], &mut retired)
            .expect("prepare same revision");
        submit_and_wait(&shared, command);
        assert_eq!(uploaded, 0, "同版本帧不得重复上传");

        for handle in retired {
            let _ = shared.release(handle);
        }
        registry.release(&shared);
        // SAFETY: 池只被本测试使用，命令缓冲已执行完毕。
        unsafe { shared.raw().destroy_command_pool(pool, None) };
    }
}
