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

pub(crate) struct GeometryEntry {
    vertex: Retained<ProtocolObject<dyn MTLBuffer>>,
    index: Retained<ProtocolObject<dyn MTLBuffer>>,
    index_count: u32,
    topology: Topology,
    pipeline: GeometryPipeline,
    index_type: IndexType,
    generation: u32,
    last_used_frame: u64,
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

const EVICT_AFTER_FRAMES: u64 = 180;

fn geometry_is_stale(frame: u64, last_used_frame: u64) -> bool {
    frame.wrapping_sub(last_used_frame) > EVICT_AFTER_FRAMES
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
                retired.extend(entry.storage);
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

fn copy_page(
    blit: &ProtocolObject<dyn MTLBlitCommandEncoder>,
    staging: &ProtocolObject<dyn MTLBuffer>,
    staging_offset: usize,
    bytes: &bytes::Bytes,
    target: &ProtocolObject<dyn MTLBuffer>,
    target_offset: usize,
) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            staging.contents().as_ptr().cast::<u8>().add(staging_offset),
            bytes.len(),
        );
    }
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

    #[test]
    fn geometry_is_evicted_only_after_the_grace_window() {
        assert!(!geometry_is_stale(1_000, 1_000));
        assert!(!geometry_is_stale(1_000 + EVICT_AFTER_FRAMES, 1_000));
        assert!(geometry_is_stale(1_001 + EVICT_AFTER_FRAMES, 1_000));
        assert!(!geometry_is_stale(0, u64::MAX));
    }

    #[test]
    fn staging_upload_runs_on_a_real_metal_device() {
        use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandQueue, MTLDevice};

        use crate::{
            math::{LogicalRect, Transform, px},
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
                bounds: LogicalRect::new(px(0.0), px(0.0), px(32.0), px(32.0)),
                pipeline: GeometryPipeline::Ui,
            })
            .expect("注册几何");
        store
            .update_vertices(id, 12..24, &[0xAB; 12])
            .expect("range 更新");
        let mut registry = super::GeometryRegistry::new();
        let mut retired = Vec::new();

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
