use windows::Win32::Graphics::{
    Direct3D12::{
        D3D12_CPU_PAGE_PROPERTY_UNKNOWN, D3D12_HEAP_FLAG_NONE, D3D12_HEAP_PROPERTIES,
        D3D12_HEAP_TYPE_UPLOAD, D3D12_MEMORY_POOL_UNKNOWN, D3D12_RESOURCE_DESC,
        D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_STATE_GENERIC_READ,
        D3D12_TEXTURE_LAYOUT_ROW_MAJOR, ID3D12Device, ID3D12Resource,
    },
    Dxgi::Common::{DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC},
};

use super::{GpuRenderer, UploadArena, UploadSlot, on_gpu_thread};
use crate::{
    error::EngineError,
    paint::{atlas::AtlasKind, instance::GpuClipEntry},
    render::{
        frame::{FramePayload, InstanceMirror, MAX_FRAMES_IN_FLIGHT},
        retire::GpuResourceHandle,
        windows::{
            device::{SharedGpu, init_error_from},
            queue::CommandStream,
        },
    },
};

pub(super) fn queue_atlas_upload(
    renderer: &mut GpuRenderer,
    kind: AtlasKind,
    layer: u16,
    spec: crate::render::AtlasArraySpec,
    pixels: Vec<u8>,
    rects: crate::render::atlas::DirtyRects,
) {
    debug_assert!(
        renderer.swapchain.is_none() || on_gpu_thread(),
        "window atlas uploads may only be queued on the kytrix-d3d12 thread"
    );
    let queued_page = renderer.shared.atlas_page(kind);
    renderer
        .atlas_uploads
        .queue_upload(kind, layer, spec, queued_page, pixels, rects);
}

pub(super) fn create_arena(shared: &SharedGpu) -> Result<UploadArena, EngineError> {
    let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
    for _ in 0..MAX_FRAMES_IN_FLIGHT {
        let instances = create_upload(shared.raw(), 256 * 128)?;
        let instance_handle = shared.allocate(instances.clone());
        let instance_ptr = map_upload(&instances)?;
        let clips = create_upload(shared.raw(), 8 * 48)?;
        let clip_handle = shared.allocate(clips.clone());
        let clip_ptr = map_upload(&clips)?;
        slots.push(UploadSlot {
            instances,
            instance_handle,
            instance_capacity: 256,
            instance_ptr,
            clips,
            clip_handle,
            clip_capacity: 8,
            clip_ptr,
        });
    }
    Ok(UploadArena { slots })
}

pub(crate) struct GeometryUploadArena {
    slots: Vec<GeometryUploadSlot>,
}

pub(crate) struct GeometryUploadSlot {
    resource: ID3D12Resource,
    handle: GpuResourceHandle,
    capacity: u32,
    used: u32,
    pointer: *mut u8,
}

const GEOMETRY_STAGING_INITIAL: u32 = 64 * 1024;

impl GeometryUploadArena {
    pub(crate) fn new(shared: &SharedGpu) -> Result<Self, EngineError> {
        let mut slots = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let resource = create_upload(shared.raw(), u64::from(GEOMETRY_STAGING_INITIAL))?;
            let handle = shared.allocate(resource.clone());
            let pointer = map_upload(&resource)?;
            slots.push(GeometryUploadSlot {
                resource,
                handle,
                capacity: GEOMETRY_STAGING_INITIAL,
                used: 0,
                pointer,
            });
        }
        Ok(Self { slots })
    }

    pub(crate) fn release(&self, shared: &SharedGpu) {
        for slot in &self.slots {
            let _ = shared.release(slot.handle);
        }
    }

    pub(crate) fn begin(&mut self, slot: usize) {
        if let Some(target) = self.slots.get_mut(slot) {
            target.used = 0;
        }
    }

    pub(crate) fn stage(
        &mut self,
        shared: &SharedGpu,
        slot: usize,
        bytes: &[u8],
        retired: &mut Vec<GpuResourceHandle>,
    ) -> Result<(&ID3D12Resource, u32), EngineError> {
        let target = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| EngineError::AppInit {
                detail: smol_str::SmolStr::new_static("geometry staging slot out of range"),
            })?;
        let aligned = (bytes.len() as u32).next_multiple_of(512);
        let needed = target
            .used
            .checked_add(aligned)
            .ok_or_else(|| EngineError::AppInit {
                detail: smol_str::SmolStr::new_static("geometry staging size overflow"),
            })?;
        if needed > target.capacity {
            let capacity = needed.max(GEOMETRY_STAGING_INITIAL).next_power_of_two();
            let resource = create_upload(shared.raw(), u64::from(capacity))?;
            let handle = shared.allocate(resource.clone());
            let pointer = map_upload(&resource)?;
            let old = target.handle;
            target.resource = resource;
            target.handle = handle;
            target.pointer = pointer;
            target.capacity = capacity;
            retired.push(old);
        }
        let offset = target.used;
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                target.pointer.add(offset as usize),
                bytes.len(),
            );
        }
        target.used += aligned;
        Ok((&target.resource, offset))
    }
}

pub(super) fn write_uploads(
    arena: &mut UploadArena,
    mirror: &mut InstanceMirror,
    shared: &SharedGpu,
    stream: &mut CommandStream,
    sequence: u64,
    slot: usize,
    payload: &FramePayload,
) -> Result<(u32, u32), EngineError> {
    let instance_count = payload.instance_count();
    let clips = payload.clips();
    let clip_count = clips.len() as u32;
    let needs_instances = instance_count > arena.slots[slot].instance_capacity;
    if needs_instances {
        let capacity = instance_count.next_power_of_two().max(256);
        let resource = create_upload(shared.raw(), u64::from(capacity) * 128)?;
        let handle = shared.allocate(resource.clone());
        let pointer = map_upload(&resource)?;
        let old = arena.slots[slot].instance_handle;
        arena.slots[slot].instances = resource;
        arena.slots[slot].instance_handle = handle;
        arena.slots[slot].instance_capacity = capacity;
        arena.slots[slot].instance_ptr = pointer;
        stream.retire(sequence, old);
        mirror.invalidate(slot);
    }
    let needs_clips = clip_count > arena.slots[slot].clip_capacity;
    if needs_clips {
        let capacity = clip_count.next_power_of_two().max(8);
        let resource = create_upload(shared.raw(), u64::from(capacity) * 48)?;
        let handle = shared.allocate(resource.clone());
        let pointer = map_upload(&resource)?;
        let old = arena.slots[slot].clip_handle;
        arena.slots[slot].clips = resource;
        arena.slots[slot].clip_handle = handle;
        arena.slots[slot].clip_capacity = capacity;
        arena.slots[slot].clip_ptr = pointer;
        stream.retire(sequence, old);
    }

    let full = mirror.prepare(slot, payload);
    let instance_ptr = arena.slots[slot].instance_ptr;
    unsafe {
        if full {
            let bytes = &mirror.bytes()[..instance_count as usize * 128];
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), instance_ptr, bytes.len());
        } else {
            for range in mirror.writes() {
                let start = range.start as usize * 128;
                let end = range.end as usize * 128;
                std::ptr::copy_nonoverlapping(
                    mirror.bytes()[start..end].as_ptr(),
                    instance_ptr.add(start),
                    end - start,
                );
            }
        }
        std::ptr::copy_nonoverlapping(
            bytemuck::cast_slice::<GpuClipEntry, u8>(clips).as_ptr(),
            arena.slots[slot].clip_ptr,
            clip_count as usize * 48,
        );
    }
    mirror.finish(slot);
    Ok((instance_count, clip_count))
}

pub(super) fn map_upload(resource: &ID3D12Resource) -> Result<*mut u8, EngineError> {
    let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
    unsafe { resource.Map(0, None, Some(&mut pointer)) }
        .map_err(|error| init_error_from("Map(upload)", error))?;
    Ok(pointer.cast::<u8>())
}

pub(super) fn create_upload(
    device: &ID3D12Device,
    bytes: u64,
) -> Result<ID3D12Resource, EngineError> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.max(4),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..D3D12_RESOURCE_DESC::default()
    };
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_UPLOAD,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let mut out: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut out,
        )
    }
    .map_err(|error| init_error_from("CreateCommittedResource(upload)", error))?;
    out.ok_or_else(|| EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("d3d12 upload buffer missing"),
    })
}
