//! 图集 / 实例上传：`UploadArena`（每 in-flight 槽的实例与裁剪缓冲）、staging 资源创建、
//! 图集上传入队。实现体由 `compositor.rs` 的薄委托调用（零行为变更的结构拆分）。

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

/// 消费者：`GpuRenderer::queue_atlas_upload`（`compositor.rs` 的薄委托）。
/// 图集上传入口：字形页与图片页共用（GPU 线程消费）。
///
/// `pixels` 是 `rects` 顺序逐矩形紧密打包的字节（整页上传时就是整页），按**所有权**移动进待上传槽，
/// 避免槽 → 纹理之间再复制一次（DEVIATIONS F-211）。`layer` 是图集 texture array 的层号
/// （字形恒 0、图片 = 图集页索引），`extent` 是该层页边长。入队时同时记录**当前共享页**
/// （`SharedGpu::atlas_page`），供录制前区分「App 换代的新页」与「已被别人换代的旧页」。
pub(super) fn queue_atlas_upload(
    renderer: &mut GpuRenderer,
    kind: AtlasKind,
    layer: u16,
    extent: u32,
    pixels: Vec<u8>,
    rects: crate::render::atlas::DirtyRects,
) {
    debug_assert!(
        renderer.swapchain.is_none() || on_gpu_thread(),
        "window atlas uploads may only be queued on the kytrix-d3d12 thread"
    );
    let queued_extent = renderer.shared.atlas_page(kind);
    renderer
        .atlas_uploads
        .queue_upload(kind, layer, extent, queued_extent, pixels, rects);
}

/// 消费者：`UploadArena::new`（`compositor.rs` 的薄委托）。
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

/// 几何页级上传的 staging arena（X5）：每 in-flight 槽一块持久映射的 UPLOAD 缓冲，帧内
/// bump 分配、帧初重置。
///
/// 槽的复用安全性由既有帧同步保证：`submit_recorded` 在录制前已经等过该槽的 fence，
/// 因此本帧覆写缓冲区时，该槽上一帧的命令已完成。
pub(crate) struct GeometryUploadArena {
    slots: Vec<GeometryUploadSlot>,
}

pub(crate) struct GeometryUploadSlot {
    resource: ID3D12Resource,
    handle: GpuResourceHandle,
    /// 容量（字节；按 64 KiB 对齐增长）。
    capacity: u32,
    /// 本帧已用字节（512 B 对齐的 bump 指针）。
    used: u32,
    pointer: *mut u8,
}

/// staging 缓冲增长率：首块 64 KiB，之后翻倍（与几何缓冲同一对齐粒度，避免小更新反复分配）。
const GEOMETRY_STAGING_INITIAL: u32 = 64 * 1024;

impl GeometryUploadArena {
    /// 每 in-flight 槽一块 staging 缓冲（消费者：`GpuRenderer::new`）。
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

    /// teardown：把 staging 缓冲的句柄交回共享设备。
    pub(crate) fn release(&self, shared: &SharedGpu) {
        for slot in &self.slots {
            let _ = shared.release(slot.handle);
        }
    }

    /// 帧初重置 bump 指针（调用方保证该槽上一帧已完成）。
    pub(crate) fn begin(&mut self, slot: usize) {
        if let Some(target) = self.slots.get_mut(slot) {
            target.used = 0;
        }
    }

    /// 把 `bytes` 拷进 staging 缓冲，返回 `(staging 资源, 偏移)`（供 `CopyBufferRegion`）。
    ///
    /// 每段按 512 B 对齐（`D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT` 下限；buffer copy 的
    /// 对齐要求远低于此，取它是为了保守覆盖驱动的实现差异）。
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
        // SAFETY: bump 区间在容量内（上面已扩容）；`bytes` 非空时源指针有效。
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

/// 消费者：`UploadArena::write`（`compositor.rs` 的薄委托）。
/// 写入本帧的实例/裁剪字节；容量不够时扩容（旧缓冲交 retire 队列）。返回 `(实例数, 裁剪层数)`。
///
/// **实例是增量写**：`InstanceMirror` 持有完整实例字节，本帧只把「变化区间」（或整块，当该槽
/// 需要整块重写时）拷进当前槽的 UPLOAD 缓冲；未变化的段保留该槽上次的内容。裁剪表仍然全量
/// 写（它是小表，且表项索引必须与实例里内嵌的 `clip_index` 同帧一致）。
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
        // 新缓冲内容未定义：这个槽的增量欠账全部作废，下一次必须整块重写。
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
    // SAFETY: 槽缓冲的容量已按本帧实例数扩容（`instance_count × 128`），写入区间都在容量内；
    // 指针是创建时的持久映射（D3D12 约定：UPLOAD 堆的 Map 只需一次，之后一直可写）。
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

/// 消费者：`create_upload` 与 `UploadArena` 的槽创建。
/// 把一块 UPLOAD 堆资源持久映射一次，返回长期可写的 CPU 指针（DEVIATIONS F-210）。
///
/// D3D12 允许对 UPLOAD 堆 `Map` 一次后长期保留指针（`Unmap` 只表示 CPU 写入结束，GPU 侧
/// 不受影响）；资源释放时映射随之失效，不需要显式 `Unmap`。
pub(super) fn map_upload(resource: &ID3D12Resource) -> Result<*mut u8, EngineError> {
    let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: 资源是 `D3D12_HEAP_TYPE_UPLOAD`，整个生命周期只由本函数映射一次。
    unsafe { resource.Map(0, None, Some(&mut pointer)) }
        .map_err(|error| init_error_from("Map(upload)", error))?;
    Ok(pointer.cast::<u8>())
}

/// 消费者：`UploadArena::new` / `UploadArena::write`。
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
    // SAFETY: COM 调用：设备 / 工厂 / swapchain 句柄有效，参数与输出指针在调用期间存活，失败按 HRESULT 处理。
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
