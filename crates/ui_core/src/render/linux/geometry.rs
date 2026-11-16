//! Linux 的静态几何 GPU 缓冲（`GeometryId → 顶点/索引缓冲`，按需创建 + 按最近使用驱逐）。
//!
//! 语义与 Windows / macOS 的 `GeometryRegistry` 逐项一致：首次见到某个 `GeometryId`（或槽位
//! 换代）时按注册描述建缓冲，之后命中缓存；换代产生的旧缓冲交 retire 队列延迟释放（I4）。
//!
//! **驱逐（D-1）**：条目按 `(index, generation)` 缓存，只在代际不匹配时重建；如果应用释放了几何
//! 句柄、或长期不再绘制某个几何，GPU 缓冲会永久驻留 → 显存无界增长。修法照 Chrome 的
//! discardable GPU cache：每个条目记「最后一次被绘制的帧号」，registry 自带单调帧号
//! （每个内容帧录制前 [`GeometryRegistry::begin_frame`] 自增），帧末
//! [`GeometryRegistry::retain_recent`] 驱逐连续 [`EVICT_AFTER_FRAMES`] 帧未被绘制的条目，
//! 设备对象推进现有 retire 队列（等该帧完成后再真正释放）。
//!
//! 与 Windows 的差别只有内存类型：Vulkan 没有「UPLOAD 堆」这一说，这里用
//! `HOST_VISIBLE | HOST_COHERENT` 的设备本地缓冲，装载期 `map` 一次写完后 `unmap`
//! （几何是静态数据，之后只读）。

use ash::vk;

use crate::{
    error::EngineError,
    paint::geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType},
    render::{
        linux::device::{GpuObject, SharedGpu, init_error},
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
    vertex: vk::Buffer,
    index: vk::Buffer,
    index_count: u32,
    /// 图元类型（Vulkan 把它烘进管线状态，绘制时不需要单独绑定，因此只作登记用）。
    pipeline: GeometryPipeline,
    /// 索引格式（`cmd_bind_index_buffer` 需要）。
    index_type: vk::IndexType,
    /// 该条目对应的代际（槽位复用后必须重建缓冲）。
    generation: u32,
    /// 最后一次被绘制的帧号（[`GeometryRegistry`] 的单调帧号；驱逐判据）。
    last_used_frame: u64,
    /// 卸载时交还共享设备的句柄（顶点 buffer / memory、索引 buffer / memory，按销毁顺序）。
    storage: Vec<GpuResourceHandle>,
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

    pub(crate) fn pipeline(&self) -> GeometryPipeline {
        self.pipeline
    }

    /// 索引格式。
    pub(crate) fn index_type(&self) -> vk::IndexType {
        self.index_type
    }
}

/// `GeometryId → GPU 缓冲`（按需创建；代际不匹配即重建；长期未绘制即驱逐）。
pub(crate) struct GeometryRegistry {
    entries: Vec<Option<GeometryEntry>>,
    /// 单调帧号：每个内容帧录制前自增，`last_used_frame` 与驱逐都基于它。
    frame: u64,
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

    /// 驱逐长期未绘制的几何，把设备对象句柄交给调用方 retire。
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

    /// teardown：把本窗口几何缓冲的句柄交回共享设备（窗口 GPU 线程已等到自己的提交完成）。
    pub(crate) fn release(&self, shared: &SharedGpu) {
        for entry in self.entries.iter().flatten() {
            for handle in &entry.storage {
                let _ = shared.release(*handle);
            }
        }
    }

    /// 取该句柄的 GPU 缓冲；没有或代际不匹配时按 `desc` 现场创建。
    ///
    /// `retired` 收集被替换掉的旧句柄（调用方在本帧录制结束后交给 retire 队列，I4）。
    pub(crate) fn ensure(
        &mut self,
        shared: &SharedGpu,
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
            let entry = upload_geometry(shared, id, desc)?;
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

/// 按注册描述创建顶点/索引缓冲（host-visible；静态数据一次写入）。
fn upload_geometry(
    shared: &SharedGpu,
    id: GeometryId,
    desc: &GeometryDesc,
) -> Result<GeometryEntry, EngineError> {
    let (vertex, mut vertex_storage) = upload_buffer(shared, &desc.vertex_bytes)?;
    let (index, index_storage) = upload_buffer(shared, &desc.index_bytes)?;
    vertex_storage.extend(index_storage);
    Ok(GeometryEntry {
        vertex,
        index,
        index_count: (desc.index_bytes.len() as u32) / desc.index_type.size(),
        pipeline: desc.pipeline,
        index_type: index_type(desc.index_type),
        generation: id.generation(),
        // 由 `ensure` 在返回前立刻写成当前帧号；这里只是构造初值。
        last_used_frame: 0,
        storage: vertex_storage,
    })
}

/// 建一块 host-visible 顶点/索引缓冲并写入 `bytes`。返回 `(buffer, [buffer 句柄, memory 句柄])`。
fn upload_buffer(
    shared: &SharedGpu,
    bytes: &[u8],
) -> Result<(vk::Buffer, Vec<GpuResourceHandle>), EngineError> {
    let size = bytes.len().max(4) as u64;
    let create = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::INDEX_BUFFER)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    // SAFETY: 句柄与参数都由本函数独占，调用期间有效。
    let buffer = unsafe { shared.raw().create_buffer(&create, None) }
        .map_err(|error| init_error("create_buffer(geometry)", format!("{error:?}")))?;
    // SAFETY: 只读查询；句柄有效。
    let requirements = unsafe { shared.raw().get_buffer_memory_requirements(buffer) };
    let Some(memory_type) = shared.memory_type(
        requirements.memory_type_bits,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    ) else {
        // SAFETY: buffer 已创建且尚未交给任何人。
        unsafe { shared.raw().destroy_buffer(buffer, None) };
        return Err(init_error("geometry memory", "no host-visible memory type"));
    };
    // SAFETY: 句柄与参数都由本函数独占。
    let memory = match unsafe {
        shared.raw().allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type),
            None,
        )
    } {
        Ok(memory) => memory,
        Err(error) => {
            // SAFETY: buffer 已创建且尚未交给任何人。
            unsafe { shared.raw().destroy_buffer(buffer, None) };
            return Err(init_error(
                "allocate_memory(geometry)",
                format!("{error:?}"),
            ));
        }
    };
    // SAFETY: 目标句柄已按 Vulkan 校验规则创建，绑定范围在其分配内。
    if let Err(error) = unsafe { shared.raw().bind_buffer_memory(buffer, memory, 0) } {
        // SAFETY: 两个句柄都还没有交给任何人。
        unsafe {
            shared.raw().destroy_buffer(buffer, None);
            shared.raw().free_memory(memory, None);
        }
        return Err(init_error(
            "bind_buffer_memory(geometry)",
            format!("{error:?}"),
        ));
    }
    // SAFETY: 映射区间覆盖 `size`，写入后立即 `unmap`（静态数据，之后只读）。
    unsafe {
        let pointer = shared
            .raw()
            .map_memory(memory, 0, size, vk::MemoryMapFlags::empty())
            .map_err(|error| init_error("map_memory(geometry)", format!("{error:?}")))?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.cast::<u8>(), bytes.len());
        shared.raw().unmap_memory(memory);
    }
    // 句柄按销毁顺序登记：buffer 先于绑定它的 memory。
    let handles = vec![
        shared.allocate(GpuObject::Buffer(buffer)),
        shared.allocate(GpuObject::Memory(memory)),
    ];
    Ok((buffer, handles))
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

    use super::{EVICT_AFTER_FRAMES, GeometryEntry, GeometryRegistry, geometry_is_stale};
    use crate::{paint::geometry::GeometryPipeline, render::retire::GpuResourceHandle};

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

    fn handle(index: u32) -> GpuResourceHandle {
        GpuResourceHandle::new(1, 0, index)
    }

    /// 测试用的假条目：不建真实设备，只验证 `retain_recent` 的选择与句柄收集。
    fn entry(last_used_frame: u64, handles: Vec<GpuResourceHandle>) -> GeometryEntry {
        GeometryEntry {
            vertex: vk::Buffer::null(),
            index: vk::Buffer::null(),
            index_count: 0,
            pipeline: GeometryPipeline::Ui,
            index_type: vk::IndexType::UINT16,
            generation: 0,
            last_used_frame,
            storage: handles,
        }
    }

    /// registry 级驱逐：只驱逐超过门槛的条目，句柄按条目顺序进入 retire 列表。
    #[test]
    fn retain_recent_evicts_only_stale_entries() {
        let mut registry = GeometryRegistry::new();
        registry.frame = 1_000;
        registry.entries = vec![
            Some(entry(819, vec![handle(1), handle(2)])),
            Some(entry(820, vec![handle(3)])),
            Some(entry(900, vec![handle(4)])),
        ];
        let mut retired = Vec::new();
        registry.retain_recent(&mut retired);

        assert!(registry.entries[0].is_none(), "200 帧未绘制 ⇒ 驱逐");
        assert!(registry.entries[1].is_some(), "恰好 180 帧 ⇒ 保留");
        assert!(registry.entries[2].is_some(), "100 帧未绘制 ⇒ 保留");
        assert_eq!(retired, vec![handle(1), handle(2)]);
    }

    /// `begin_frame` 单调推进帧号（`ensure` 的 `last_used_frame` 与驱逐都基于它）。
    #[test]
    fn begin_frame_advances_the_registry_clock() {
        let mut registry = GeometryRegistry::new();
        assert_eq!(registry.frame, 0);
        registry.begin_frame();
        registry.begin_frame();
        assert_eq!(registry.frame, 2);
    }

    /// 真实设备路径：`ensure` 把缓冲登记进共享设备句柄表并记录使用帧；命中缓存不重建；
    /// 换代 / teardown 的句柄都能交回（关窗不泄漏）。
    #[test]
    fn registry_registers_and_releases_shared_device_handles() {
        use bytes::Bytes;

        use crate::{
            math::Rect,
            paint::geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
            render::linux::device::gpu_test_lock,
        };

        let _guard = gpu_test_lock();
        let shared = crate::render::linux::device::SharedGpu::new_headless_isolated()
            .expect("headless shared gpu");
        let desc = GeometryDesc {
            vertex_bytes: Bytes::from_static(&[0u8; 24]),
            index_bytes: Bytes::from_static(&[0, 0, 1, 0, 2, 0]),
            vertex_stride: 8,
            index_type: IndexType::U16,
            topology: Topology::TriangleList,
            bounds: Rect::new(0.0, 0.0, 8.0, 8.0),
            pipeline: GeometryPipeline::Ui,
        };
        let mut registry = GeometryRegistry::new();
        registry.begin_frame();
        let mut retired = Vec::new();
        {
            let entry = registry
                .ensure(&shared, GeometryId::for_test(3, 1), &desc, &mut retired)
                .expect("upload geometry");
            assert_eq!(entry.index_count(), 3);
        }
        assert!(retired.is_empty(), "首次创建不产生 retired 句柄");
        assert_eq!(
            shared.live_resources(),
            4,
            "顶点 buffer + memory、索引 buffer + memory"
        );
        assert_eq!(
            registry.entries[3].as_ref().expect("entry").last_used_frame,
            1
        );

        // 同代际命中缓存：不重建，只把使用帧号推进。
        registry.begin_frame();
        registry
            .ensure(&shared, GeometryId::for_test(3, 1), &desc, &mut retired)
            .expect("cache hit");
        assert!(retired.is_empty());
        assert_eq!(
            registry.entries[3].as_ref().expect("entry").last_used_frame,
            2
        );

        // 槽位换代：旧缓冲的句柄进 retired，新缓冲重新登记。
        registry.begin_frame();
        registry
            .ensure(&shared, GeometryId::for_test(3, 2), &desc, &mut retired)
            .expect("rebuilt geometry");
        assert_eq!(retired.len(), 4);
        assert_eq!(
            shared.live_resources(),
            8,
            "新旧两代缓冲同时驻留到旧句柄释放"
        );

        // teardown：当前条目 + retired 的旧句柄都要注销。
        registry.release(&shared);
        for handle in retired {
            assert!(shared.release(handle), "retired handle must be live");
        }
        assert_eq!(shared.live_resources(), 0);
    }
}
