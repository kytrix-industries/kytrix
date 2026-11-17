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
//! 与 Windows 的差别只有内存来源：Vulkan 没有「UPLOAD 堆」这一说，这里走 `SharedGpu` 的
//! **两级子分配器**（R11-6）：
//!
//! - 类型选择按「主机可写 + **设备本地优先**」求交（[`MemoryNeed::HOST_WRITABLE`]）：
//!   BAR / UMA 上几何直接落在设备内存（vertex fetch 走 L2），没有 BAR 的独显才退回主机堆。
//!   旧实现写死 `HOST_VISIBLE | HOST_COHERENT` 且只取「第一个匹配的类型」——在所有独显上
//!   顶点/索引都落在系统内存里，每次 draw 的 fetch 都走 PCIe（还与注释自述的「设备本地缓冲」不符）。
//! - 分配走子分配块：几何条目按 `GeometryId.index()` 增长，几百到上千条是常态，每条两次
//!   `vkAllocateMemory` 会撞上 `maxMemoryAllocationCount` 的规范下限（4096）。
//!
//! 装载期写入是**持久映射**（整块 map 一次，子分配只拿 offset），不再每条 map/unmap。

use ash::vk;

use crate::{
    error::EngineError,
    paint::geometry::{GeometryDesc, GeometryId, GeometryPipeline, IndexType, Topology},
    render::{
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
                retired.extend(entry.handles());
            }
        }
    }

    /// teardown：把本窗口几何缓冲的句柄交回共享设备（窗口 GPU 线程已等到自己的提交完成）。
    pub(crate) fn release(&self, shared: &SharedGpu) {
        for entry in self.entries.iter().flatten() {
            for handle in entry.handles() {
                let _ = shared.release(handle);
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
                retired.extend(old.handles());
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

/// 按注册描述创建顶点/索引缓冲（子分配 + 持久映射写入）。
fn upload_geometry(
    shared: &SharedGpu,
    id: GeometryId,
    desc: &GeometryDesc,
) -> Result<GeometryEntry, EngineError> {
    let vertex = upload_buffer(shared, &desc.vertex_bytes)?;
    let index = upload_buffer(shared, &desc.index_bytes)?;
    Ok(GeometryEntry {
        vertex,
        index,
        index_count: (desc.index_bytes.len() as u32) / desc.index_type.size(),
        pipeline: desc.pipeline,
        topology: desc.topology,
        index_type: index_type(desc.index_type),
        generation: id.generation(),
        // 由 `ensure` 在返回前立刻写成当前帧号；这里只是构造初值。
        last_used_frame: 0,
    })
}

/// 从子分配器拿一块顶点/索引缓冲并写入 `bytes`（静态数据，写入后不再改）。
fn upload_buffer(shared: &SharedGpu, bytes: &[u8]) -> Result<GpuBuffer, EngineError> {
    let buffer = shared.allocate_buffer(
        bytes.len().max(4) as u64,
        vk::BufferUsageFlags::VERTEX_BUFFER | vk::BufferUsageFlags::INDEX_BUFFER,
        MemoryNeed::HOST_WRITABLE,
    )?;
    let Some(pointer) = buffer.pointer else {
        // `HOST_WRITABLE` 要求 `HOST_VISIBLE`，子分配器因此必然给出映射；走到这里说明不变量被破坏。
        let _ = shared.release(buffer.handle);
        return Err(init_error(
            "geometry memory",
            "suballocated geometry buffer has no host mapping",
        ));
    };
    // SAFETY: 映射范围覆盖整个块，本缓冲的写入区间落在自己的子分配内。
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
    }
    Ok(buffer)
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
    use crate::{
        paint::geometry::{GeometryPipeline, Topology},
        render::{linux::device::GpuBuffer, retire::GpuResourceHandle},
    };

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

    /// 测试用的假缓冲：不建真实设备，只验证句柄收集。
    fn gpu_buffer(handle: GpuResourceHandle) -> GpuBuffer {
        GpuBuffer {
            buffer: vk::Buffer::null(),
            pointer: None,
            handle,
        }
    }

    /// 测试用的假条目：不建真实设备，只验证 `retain_recent` 的选择与句柄收集。
    fn entry(last_used_frame: u64, handles: [GpuResourceHandle; 2]) -> GeometryEntry {
        GeometryEntry {
            vertex: gpu_buffer(handles[0]),
            index: gpu_buffer(handles[1]),
            index_count: 0,
            pipeline: GeometryPipeline::Ui,
            topology: Topology::TriangleList,
            index_type: vk::IndexType::UINT16,
            generation: 0,
            last_used_frame,
        }
    }

    /// registry 级驱逐：只驱逐超过门槛的条目，句柄按条目顺序进入 retire 列表。
    #[test]
    fn retain_recent_evicts_only_stale_entries() {
        let mut registry = GeometryRegistry::new();
        registry.frame = 1_000;
        registry.entries = vec![
            Some(entry(819, [handle(1), handle(2)])),
            Some(entry(820, [handle(3), handle(4)])),
            Some(entry(900, [handle(5), handle(6)])),
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
            vertex_stride: 12,
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
            2,
            "顶点缓冲 + 索引缓冲各一个子分配句柄（内存块由内存池持有，不进句柄表）"
        );
        // R11-6：顶点与索引两次小分配共用**同一块** `vkAllocateMemory`（而不是每资源一次）。
        let (device_allocations, live_bytes, reserved_bytes) = shared.memory_stats();
        assert_eq!(
            device_allocations, 1,
            "顶点与索引必须子分配在同一块设备内存里"
        );
        // 记账 = 两次 `VkMemoryRequirements.size` 之和。驱动允许把 size 放大到对齐要求，因此
        // 只能断言下界（= 请求的 24 B 顶点 + 6 B 索引）；上界用来抓「重复记账 / 按整块记账」这类
        // 回归（两个几十字节的缓冲不可能占超过 64 B 的对齐余量）。
        assert!(
            live_bytes >= 30,
            "24 B 顶点 + 6 B 索引至少占 30 B，实际 {live_bytes}"
        );
        assert!(
            live_bytes <= 30 + 2 * 64,
            "驱动的对齐余量不应超过每个缓冲 64 B，实际 {live_bytes}"
        );
        assert!(reserved_bytes >= live_bytes);
        let first_generation_bytes = live_bytes;
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

        // 槽位换代：旧缓冲的句柄进 retired，新缓冲重新登记（仍复用同一块内存）。
        registry.begin_frame();
        registry
            .ensure(&shared, GeometryId::for_test(3, 2), &desc, &mut retired)
            .expect("rebuilt geometry");
        assert_eq!(retired.len(), 2, "顶点 + 索引两个句柄");
        assert_eq!(
            shared.live_resources(),
            4,
            "新旧两代缓冲同时驻留到旧句柄释放"
        );
        assert_eq!(
            shared.memory_stats().0,
            1,
            "换代不得新增 vkAllocateMemory（这就是子分配器的意义）"
        );
        // 换代后旧句柄还没 release ⇒ 两块几何的记账恰好是上一代的两倍（精确判据，与驱动的
        // `VkMemoryRequirements.size` 无关）。
        assert_eq!(
            shared.memory_stats().1,
            first_generation_bytes * 2,
            "两代几何同时驻留时记账必须是单代的两倍"
        );

        // teardown：当前条目 + retired 的旧句柄都要注销。
        registry.release(&shared);
        for handle in retired {
            assert!(shared.release(handle), "retired handle must be live");
        }
        assert_eq!(shared.live_resources(), 0);
        // 句柄注销之后，子分配的**区间**也必须还回内存池（否则块内的空洞再也复用不到，
        // 几何反复换代会让 1 MiB 的块越切越碎）。
        assert_eq!(
            shared.memory_stats().1,
            0,
            "释放句柄必须把子分配区间还回内存池"
        );
    }
}
