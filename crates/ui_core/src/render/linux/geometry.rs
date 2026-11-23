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

const EVICT_AFTER_FRAMES: u64 = 180;

fn geometry_is_stale(frame: u64, last_used_frame: u64) -> bool {
    frame.wrapping_sub(last_used_frame) > EVICT_AFTER_FRAMES
}

pub(crate) struct GeometryEntry {
    vertex: GpuBuffer,
    index: GpuBuffer,
    index_count: u32,
    pipeline: GeometryPipeline,
    topology: Topology,
    index_type: vk::IndexType,
    generation: u32,
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

    pub(crate) fn topology(&self) -> Topology {
        self.topology
    }

    pub(crate) fn index_type(&self) -> vk::IndexType {
        self.index_type
    }

    fn handles(&self) -> [GpuResourceHandle; 2] {
        [self.vertex.handle, self.index.handle]
    }
}

pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
    uploads: GeometryUploadTracker,
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

    pub(crate) fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

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

    pub(crate) fn release(&self, shared: &SharedGpu) {
        for entry in self.entries.iter().flatten() {
            for handle in entry.handles() {
                let _ = shared.release(handle);
            }
        }
    }

    pub(crate) fn get(&self, id: GeometryId) -> Option<&GeometryEntry> {
        self.entries
            .get(id.index() as usize)
            .and_then(Option::as_ref)
            .filter(|entry| entry.generation == id.generation())
    }

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

fn create_geometry_buffer(shared: &SharedGpu, capacity: u32) -> Result<GpuBuffer, EngineError> {
    shared.allocate_buffer(
        u64::from(capacity.max(4)),
        vk::BufferUsageFlags::VERTEX_BUFFER
            | vk::BufferUsageFlags::INDEX_BUFFER
            | vk::BufferUsageFlags::TRANSFER_DST,
        MemoryNeed::HOST_WRITABLE,
    )
}

fn staging_buffer(shared: &SharedGpu, bytes: u64) -> Result<GpuBuffer, EngineError> {
    shared.allocate_buffer(
        bytes.max(4),
        vk::BufferUsageFlags::TRANSFER_SRC,
        MemoryNeed::HOST_WRITABLE,
    )
}

fn stage_page(pointer: &*mut u8, offset: usize, bytes: &[u8]) {
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.add(offset), bytes.len());
    }
}

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
    let barrier_out = vk::BufferMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::SHADER_READ)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(destination)
        .offset(0)
        .size(vk::WHOLE_SIZE);
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

    #[test]
    fn eviction_boundary_is_exclusive_and_wraps() {
        assert!(!geometry_is_stale(1_000, 1_000));
        assert!(!geometry_is_stale(1_000 + EVICT_AFTER_FRAMES, 1_000));
        assert!(geometry_is_stale(1_001 + EVICT_AFTER_FRAMES, 1_000));
        assert!(!geometry_is_stale(0, u64::MAX));
    }

    #[test]
    fn begin_frame_advances_the_registry_clock() {
        let mut registry = GeometryRegistry::new();
        assert_eq!(registry.frame, 0);
        registry.begin_frame();
        registry.begin_frame();
        assert_eq!(registry.frame, 2);
    }

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
        let snapshot = store.snapshot_arc(id).expect("快照");
        let snapshot_vertex_len = snapshot.vertex_len();
        let draw = GeometryDrawGpu {
            id,
            range: 0..3,
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
        let vertex_bytes = unsafe { std::slice::from_raw_parts(vertex_pointer, 36) };
        assert_eq!(&vertex_bytes[0..12], &[0x11; 12]);
        assert_eq!(&vertex_bytes[12..24], &[0xAB; 12], "增量页必须落到页内偏移");
        assert_eq!(&vertex_bytes[24..36], &[0x33; 12]);
        let index_pointer = entry.index.pointer.expect("索引缓冲必须可读回");
        let index_bytes = unsafe { std::slice::from_raw_parts(index_pointer, 6) };
        assert_eq!(index_bytes, &[0u8, 0, 1, 0, 2, 0]);
        assert!(
            snapshot_vertex_len as usize <= GEOMETRY_PAGE_BYTES,
            "最小反例应落在单页内（本测试的目标是页内偏移而不是跨页）"
        );

        let snapshot = store.snapshot_arc(id).expect("快照");
        let draw = GeometryDrawGpu {
            id,
            range: 0..3,
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
        unsafe { shared.raw().destroy_command_pool(pool, None) };
    }
}
