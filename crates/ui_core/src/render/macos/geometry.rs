//! macOS 的静态几何 GPU 缓冲（`GeometryId → MTLBuffer`，按需创建）。
//!
//! 语义与 Windows / Linux 的实现逐项一致：首次见到某个 `GeometryId`（或槽位换代）时按注册
//! 描述建缓冲，之后命中缓存；换代产生的旧缓冲交 retire 队列延迟释放（I4）。
//!
//! Metal 侧用 `StorageModeShared`：几何是静态数据，装载期写入后只读，不需要 staging 拷贝。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{MTLBuffer, MTLDevice, MTLResourceOptions};

use crate::{
    error::EngineError,
    paint::geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
    render::{
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

    /// 卸载时交还的句柄。
    pub(crate) fn storage(&self) -> [GpuResourceHandle; 2] {
        self.storage
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
    pub(crate) fn ensure(
        &mut self,
        device: &mut Device,
        id: GeometryId,
        desc: &GeometryDesc,
        retired: &mut Vec<GpuResourceHandle>,
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

/// 按注册描述创建顶点/索引缓冲（`StorageModeShared`，一次写入）。
fn upload_geometry(
    device: &mut Device,
    id: GeometryId,
    desc: &GeometryDesc,
) -> Result<GeometryEntry, EngineError> {
    let vertex = upload_buffer(device, &desc.vertex_bytes)?;
    let index = upload_buffer(device, &desc.index_bytes)?;
    let vertex_handle = device.allocate(GpuObject::Buffer(vertex.clone()));
    let index_handle = device.allocate(GpuObject::Buffer(index.clone()));
    Ok(GeometryEntry {
        vertex: vertex,
        index,
        index_count: (desc.index_bytes.len() as u32) / desc.index_type.size(),
        topology: desc.topology,
        pipeline: desc.pipeline,
        index_type: desc.index_type,
        generation: id.generation(),
        storage: [vertex_handle, index_handle],
    })
}

/// 建一块 shared 缓冲并写入 `bytes`。
fn upload_buffer(
    device: &mut Device,
    bytes: &[u8],
) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>, EngineError> {
    let buffer = device
        .raw()
        .newBufferWithLength_options(bytes.len().max(4), MTLResourceOptions::StorageModeShared)
        .ok_or_else(|| init_error("newBufferWithLength_options(geometry)", "returned nil"))?;
    // SAFETY: 缓冲容量按 `bytes.len()` 分配；shared 内存 CPU 可见；写入后不再改。
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            buffer.contents().as_ptr().cast::<u8>(),
            bytes.len(),
        );
    }
    Ok(buffer)
}
