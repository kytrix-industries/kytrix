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

/// `GeometryId → GPU 缓冲`（按需创建；代际不匹配即重建）。
pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
    /// 单调帧号：每个内容帧录制前自增，`last_used_frame` 与驱逐都基于它。
    frame: u64,
}

/// 连续多少帧未被绘制就回收该几何的 GPU 缓冲（180 帧 ≈ 60 Hz 下 3 秒）。
///
/// 足够覆盖「隔帧 / 低频动画」的几何，同时让被应用释放的静态几何在几秒内归还显存
/// （Chrome 的 discardable GPU cache 用同类按使用时间的策略）。
const EVICT_AFTER_FRAMES: u64 = 180;

/// 该几何是否已连续 `EVICT_AFTER_FRAMES` 帧以上未被绘制（`wrapping_sub` 容忍回绕）。
fn geometry_is_stale(frame: u64, last_used_frame: u64) -> bool {
    frame.wrapping_sub(last_used_frame) > EVICT_AFTER_FRAMES
}

impl GeometryRegistry {
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
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
        // 记录使用帧号：`retain_recent` 据此驱逐长期未绘制的几何（D-1）。
        if let Some(entry) = &mut self.entries[slot] {
            entry.last_used_frame = self.frame;
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
        vertex,
        index,
        index_count: (desc.index_bytes.len() as u32) / desc.index_type.size(),
        topology: desc.topology,
        pipeline: desc.pipeline,
        index_type: desc.index_type,
        generation: id.generation(),
        // 由 `ensure` 在返回前立刻写成当前帧号；这里只是构造初值。
        last_used_frame: 0,
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
}
