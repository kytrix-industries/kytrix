//! Linux 的静态几何 GPU 缓冲（`GeometryId → 顶点/索引缓冲`，按需创建）。
//!
//! 语义与 Windows 的 `render/windows/pipeline.rs::GeometryRegistry` 逐项一致：首次见到某个
//! `GeometryId`（或槽位换代）时按注册描述建缓冲，之后命中缓存；换代产生的旧缓冲交 retire
//! 队列延迟释放（I4）。
//!
//! 与 Windows 的差别只有内存类型：Vulkan 没有「UPLOAD 堆」这一说，这里用
//! `HOST_VISIBLE | HOST_COHERENT` 的设备本地缓冲，装载期 `map` 一次写完后 `unmap`
//! （几何是静态数据，之后只读）。

use ash::vk;

use crate::{
    error::EngineError,
    paint::geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
    render::linux::device::{Device, GpuObject, init_error},
};

/// 一条已注册几何的 GPU 视图。
pub(crate) struct GeometryEntry {
    vertex: vk::Buffer,
    index: vk::Buffer,
    index_count: u32,
    topology: vk::PrimitiveTopology,
    pipeline: GeometryPipeline,
    /// 索引格式（`cmd_bind_index_buffer` 需要）。
    index_type: vk::IndexType,
    /// 该条目对应的代际（槽位复用后必须重建缓冲）。
    generation: u32,
    /// 卸载时交还的设备对象（缓冲 + 内存）。
    storage: Vec<GpuObject>,
}

impl GeometryEntry {
    pub(crate) fn vertex(&self) -> vk::Buffer {
        self.vertex
    }

    pub(crate) fn index(&self) -> vk::Buffer {
        self.index
    }

    pub(crate) fn index_count(&self) -> u32 {
        self.index_count
    }

    pub(crate) fn topology(&self) -> vk::PrimitiveTopology {
        self.topology
    }

    pub(crate) fn pipeline(&self) -> GeometryPipeline {
        self.pipeline
    }

    /// 索引格式。
    pub(crate) fn index_type(&self) -> vk::IndexType {
        self.index_type
    }

    /// 卸载时交还的设备对象。
    pub(crate) fn storage(&self) -> Vec<GpuObject> {
        self.storage.clone()
    }
}

/// `GeometryId → GPU 缓冲`（按需创建；代际不匹配即重建）。
pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
}

impl GeometryRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// 取该句柄的 GPU 缓冲；没有或代际不匹配时按 `desc` 现场创建。
    ///
    /// `retired` 收集被替换掉的旧设备对象（调用方交给 retire 队列）。
    pub(crate) fn ensure(
        &mut self,
        device: &Device,
        id: GeometryId,
        desc: &GeometryDesc,
        retired: &mut Vec<GpuObject>,
    ) -> Result<&GeometryEntry, EngineError> {
        let slot = id.index() as usize;
        if self.entries.len() <= slot {
            self.entries.resize_with(slot + 1, || None);
        }
        let stale = match &self.entries[slot] {
            Some(entry) => entry.generation != id.generation(),
            None => true,
        };
        if stale {
            if let Some(old) = self.entries[slot].take() {
                retired.extend(old.storage);
            }
            let entry = upload_geometry(device, id, desc)?;
            self.entries[slot] = Some(entry);
        }
        self.entries[slot]
            .as_ref()
            .ok_or_else(|| EngineError::GeometryRegister {
                detail: smol_str::format_smolstr!(
                    "geometry {}:{} failed to produce GPU buffers",
                    id.index(),
                    id.generation()
                ),
            })
    }
}

/// 按注册描述创建顶点/索引缓冲（host-visible；静态数据一次写入）。
fn upload_geometry(
    device: &Device,
    id: GeometryId,
    desc: &GeometryDesc,
) -> Result<GeometryEntry, EngineError> {
    let vertex = upload_buffer(device, &desc.vertex_bytes)?;
    let index = upload_buffer(device, &desc.index_bytes)?;
    Ok(GeometryEntry {
        vertex: vertex.0,
        index: index.0,
        index_count: (desc.index_bytes.len() as u32) / desc.index_type.size(),
        topology: match desc.topology {
            Topology::TriangleList => vk::PrimitiveTopology::TRIANGLE_LIST,
            Topology::TriangleStrip => vk::PrimitiveTopology::TRIANGLE_STRIP,
        },
        pipeline: desc.pipeline,
        index_type: index_type(desc.index_type),
        generation: id.generation(),
        storage: vec![vertex.1, vertex.2, index.1, index.2],
    })
}

/// 建一块 host-visible 顶点/索引缓冲并写入 `bytes`。返回 `(buffer, buffer_obj, memory_obj)`。
fn upload_buffer(
    device: &Device,
    bytes: &[u8],
) -> Result<(vk::Buffer, GpuObject, GpuObject), EngineError> {
    let size = bytes.len().max(4) as u64;
    let create = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::INDEX_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: 句柄与参数都由本函数独占，调用期间有效。
    let buffer = unsafe { device.raw().create_buffer(&create, None) }
        .map_err(|error| init_error("create_buffer(geometry)", format!("{error:?}")))?;
    // SAFETY: 只读查询；句柄有效。
    let requirements = unsafe { device.raw().get_buffer_memory_requirements(buffer) };
    let memory_type = device
        .memory_type(
            requirements.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )
        .ok_or_else(|| init_error("geometry memory", "no host-visible memory type"))?;
    // SAFETY: 句柄与参数都由本函数独占。
    let memory = unsafe {
        device.raw().allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type),
            None,
        )
    }
    .map_err(|error| init_error("allocate_memory(geometry)", format!("{error:?}")))?;
    // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定范围在其分配内。
    unsafe { device.raw().bind_buffer_memory(buffer, memory, 0) }
        .map_err(|error| init_error("bind_buffer_memory(geometry)", format!("{error:?}")))?;
    // SAFETY: 映射区间覆盖 `size`，写入后立即 `unmap`（静态数据，之后只读）。
    unsafe {
        let pointer = device
            .raw()
            .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
            .map_err(|error| init_error("map_memory(geometry)", format!("{error:?}")))?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
        device.raw().unmap_memory(memory);
    }
    Ok((buffer, GpuObject::Buffer(buffer), GpuObject::Memory(memory)))
}

/// 索引格式（`vk::IndexType`）。
fn index_type(kind: IndexType) -> vk::IndexType {
    match kind {
        IndexType::U16 => vk::IndexType::UINT16,
        IndexType::U32 => vk::IndexType::UINT32,
    }
}
