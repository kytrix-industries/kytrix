//! 设备内存：**类型选择策略** + **两级子分配器**（R11-6）。
//!
//! 旧实现有两个结构性问题：
//!
//! 1. `memory_type()` 只做「`type_bits` 里第一个满足 `required` 的类型」，没有偏好、没有排除
//!    `LAZILY_ALLOCATED` / `PROTECTED`。在 `DEVICE_LOCAL` 与 `HOST_VISIBLE` 分属不同 heap 的
//!    机器（所有独显）上，顶点/索引落在**系统内存**里，每次 draw 的 vertex fetch 都走 PCIe ——
//!    与 D3D12（`DEFAULT` 堆）/ Metal（`.private`）不可比。这个问题在「引擎自述用设备本地缓冲」
//!    的注释掩护下潜伏了很久（R11-32 列的 3 条注释与实现不符之一）。
//! 2. 每个资源一次 `vkAllocateMemory`（8 处调用点）：规范只保证
//!    `maxMemoryAllocationCount >= 4096`，而几何条目按 `GeometryId.index()` 增长，几百到上千条
//!    是常态；每次几十字节的缓冲各自占一次分配，按驱动最小粒度（4–64 KiB）放大。
//!
//! 本模块只做两件事，且都是**纯决策 + 记账**（不持有队列、不发命令）：
//!
//! - [`pick_memory_type`]：按「用途 → 硬需求 + 偏好链」在 `memoryTypeBits` 里挑类型；
//! - [`MemoryArena`]：按 `(memory type, 线性/最优)` 分块（block），块内是带对齐的空闲表
//!   （slab / free-list），块用尽时才 `vkAllocateMemory`。
//!
//! 大对象（persist 图、图集页、整页 staging）仍走**专用分配**（`device::GpuObject::Memory`）：
//! 数量少、尺寸大，子分配没有收益，反而引入「一块内存被几张图共享」的隐式耦合。把子分配用在
//! 「数量无上界」的小对象（几何缓冲、每窗口上传缓冲）上，正是 wgpu / vulkano / Dawn 的取舍。

use ash::vk;

use crate::{error::EngineError, render::linux::device::init_error};

/// 一次分配的内存需求（用途，而不是 Vulkan 的 `VkMemoryRequirements`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryNeed {
    /// 主机必须可写（staging / 上传缓冲 / 几何直写）。
    pub host_visible: bool,
    /// 主机写入必须**不需要** flush（`HOST_COHERENT`）。
    ///
    /// Vulkan 保证至少存在一个 `HOST_VISIBLE | HOST_COHERENT` 类型，因此它可以是硬需求：
    /// 本引擎没有任何「非一致内存 + `vkFlushMappedMemoryRanges`」路径，放宽它会让写入
    /// 静默丢失（`vkMapMemory` 之后 CPU 写入不保证对 GPU 可见）。
    pub host_coherent: bool,
    /// 设备本地优先（顶点/索引/采样纹理：要 L2 复用，避免每次 fetch 走 PCIe）。
    pub device_local: bool,
}

impl MemoryNeed {
    /// 主机要写、且拿设备本地更好：几何缓冲、每帧上传缓冲（BAR / UMA 上顺带拿 L2）。
    pub(crate) const HOST_WRITABLE: Self = Self {
        host_visible: true,
        host_coherent: true,
        device_local: true,
    };

    /// 读回缓冲：主机可读、必须一致；**不要求**设备本地（读回本来就要把像素搬过总线，占 BAR
    /// 窗口纯属浪费 —— 见 [`pick_memory_type`] 的打分规则）。
    pub(crate) const READBACK: Self = Self {
        host_visible: true,
        host_coherent: true,
        device_local: false,
    };

    /// 纯设备侧图像（persist / 图集页）。
    pub(crate) const DEVICE_IMAGE: Self = Self {
        host_visible: false,
        host_coherent: false,
        device_local: true,
    };
}

/// 一个候选内存类型的属性（`VkPhysicalDeviceMemoryProperties::memoryTypes[i].propertyFlags`）。
///
/// 单独取标志位切片而不是整个 `VkPhysicalDeviceMemoryProperties`，是为了让选择逻辑可以脱离
/// 设备实例做单元测试。
pub(crate) type MemoryTypeFlags = [vk::MemoryPropertyFlags];

/// 不可能用来承载普通资源的属性：懒分配（transient attachment 专用）与保护内存（受保护队列
/// 专用）。选到它们会得到「分配成功但用不了」或直接 `ERROR_OUT_OF_DEVICE_MEMORY`。
/// 懒分配（transient attachment 专用）与保护内存（受保护队列专用）都不得承载普通资源。
const LAZILY_ALLOCATED: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::LAZILY_ALLOCATED;
const PROTECTED: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::PROTECTED;

/// 在 `type_bits` 允许的类型里挑一个（R11-6）。
///
/// 规则（与 Dawn 的 `MemoryAllocator` / vulkano 的 `MemoryTypePool` 同一形状）：
/// 1. 先按**硬需求**过滤：`host_visible` ⇒ `HOST_VISIBLE`、`host_coherent` ⇒ `HOST_COHERENT`；
///    同时排除 `LAZILY_ALLOCATED` / `PROTECTED`。
/// 2. 再按**偏好**打分：
///    - `need.device_local` 且类型是 `DEVICE_LOCAL` ⇒ **最高分**（顶点/索引/纹理不必走 PCIe）；
///    - 反之（不需要设备本地，例如读回）`DEVICE_LOCAL|HOST_VISIBLE` 是 BAR 窗口，**扣分**：
///      它稀缺，而读回反正要把数据搬过总线，普通主机堆更合适（wgpu 的 `CpuToGpu`/`GpuToCpu`
///      同样只要求 host-visible + coherent，不要求 device-local）；
///    - 纯设备资源落在主机可见堆上同样扣分（BAR 预算）；
///    - `HOST_COHERENT` 加分（省一次 flush/invalidate）。
/// 3. 同分取 index 最小者（稳定、可测）。
///
/// 没有任何候选时返回 `None`（调用方给出强类型错误，不做静默回退）。
pub(crate) fn pick_memory_type(
    type_bits: u32,
    types: &MemoryTypeFlags,
    need: MemoryNeed,
) -> Option<u32> {
    let mut best: Option<(i32, u32)> = None; // (score, index)
    for (index, flags) in types.iter().enumerate() {
        let index = index as u32;
        if type_bits & (1 << index) == 0 {
            continue;
        }
        if flags.contains(LAZILY_ALLOCATED) || flags.contains(PROTECTED) {
            continue;
        }
        if need.host_visible && !flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            continue;
        }
        if need.host_coherent && !flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT) {
            continue;
        }
        let mut score = 0i32;
        if flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL) {
            if need.device_local {
                // 唯一的「性能」维度：设备本地意味着顶点/索引/纹理不必走 PCIe。
                score += 4;
            } else {
                // BAR 窗口是稀缺资源；「反正要走总线」的用途不该占它。
                score -= 2;
            }
        }
        if !need.host_visible && flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            // 纯设备资源落在主机可见堆上通常是 BAR 窗口：能用，但显存预算更紧张。
            score -= 2;
        }
        if flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT) {
            score += 1;
        }
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, index));
        }
    }
    best.map(|(_, index)| index)
}

// 子分配的粒度契约（**只放线性资源**）。
//
// `VkPhysicalDeviceLimits::bufferImageGranularity` 要求线性资源与最优资源在同一块内存里彼此
// 隔离（否则相邻页可能落到同一个 GPU 页上，读写互相污染）。本模块的块因此**只承载
// `VkBuffer`**；图像/纹理（persist、图集页、staging 大块）走 `vkAllocateMemory` 的专用分配，
// 从根上避开粒度问题（vulkano 的 `MemoryTypePool` 用按 kind 分块达到同一目的）。将来若把图像
// 接进子分配，必须同时按 `bufferImageGranularity` 隔离块，而不是在同块里混排。

/// 块的最小尺寸：几何缓冲只有几 KiB，但一次 `vkAllocateMemory` 的系统开销远大于它；
/// 1 MiB 是驱动 minimum allocation granularity 的常见上界，一次分配能容纳几百个几何条目。
const MIN_BLOCK_BYTES: u64 = 1 << 20;

/// 一块设备内存 + 块内空闲表。
struct Block {
    memory: vk::DeviceMemory,
    /// 块总字节数。
    size: u64,
    /// `HOST_VISIBLE` 块的**持久映射**基址（整块 map 一次；子分配只要 `base + offset`）。
    ///
    /// 存 `usize` 而不是 `*mut u8`：原始指针会让整个 `MemoryArena` 变成 `!Send`，而它由
    /// 进程级 `SharedGpu` 的 `Mutex` 保护、要被多个窗口的 GPU 线程共享。
    mapped: usize,
    /// 空闲区间，按 offset 升序且**两两不相邻**（`give_back` 时合并）。
    free: Vec<FreeRange>,
    /// 本块当前分配出去的字节数。
    live: u64,
}

/// 块内的空闲区间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FreeRange {
    offset: u64,
    size: u64,
}

impl Block {
    /// 在块内切一块 `size`（按 `align` 向上对齐）。返回相对块起点的 offset。
    fn take(&mut self, size: u64, align: u64) -> Option<u64> {
        let align = align.max(1);
        // 先只读地找「第一个装得下的空闲区间」，再改表（避免同时借用两个方向）。
        let found = self.free.iter().enumerate().find_map(|(index, range)| {
            let start = align_up(range.offset, align)?;
            let padding = start - range.offset;
            if padding > range.size {
                return None;
            }
            let available = range.size - padding;
            (available >= size).then_some((index, start, padding))
        });
        let (index, start, padding) = found?;
        let range = self.free[index];
        // 切出去之后：区间左端（对齐填充）与右端（剩余）都回到空闲表。
        let rest_size = range.size - padding - size;
        self.free.remove(index);
        let mut inserted = 0;
        if padding > 0 {
            self.free.insert(
                index,
                FreeRange {
                    offset: range.offset,
                    size: padding,
                },
            );
            inserted += 1;
        }
        if rest_size > 0 {
            self.free.insert(
                index + inserted,
                FreeRange {
                    offset: start + size,
                    size: rest_size,
                },
            );
        }
        self.live += size;
        Some(start)
    }

    /// 归还一个区间，并与相邻空闲区间合并。
    ///
    /// 返回 `false` = 该区间与空闲表里的某个区间**重叠**（重复释放 / 伪造区间）：空闲表**不变**。
    /// 没有这道检查时，同一段字节会被归还两次并合并进空闲表，随后被两次 `take` 交给两个使用者
    /// （R11-6 的 P2，AR3 §5.5）。合法的归还永远不会重叠：`take` 会把切出去的区间从空闲表移走。
    fn give_back(&mut self, offset: u64, size: u64) -> bool {
        debug_assert!(size > 0, "a zero-sized suballocation is never handed out");
        debug_assert!(
            offset + size <= self.size,
            "suballocation {offset}+{size} exceeds block {}",
            self.size
        );
        let end = offset + size;
        // 空闲表按 offset 升序（`take` / `give_back` 都保持有序）：相交 ⇔ 左侧起点 < 右侧终点。
        if self
            .free
            .iter()
            .any(|range| range.offset < end && offset < range.offset + range.size)
        {
            return false;
        }
        let position = self.free.partition_point(|range| range.offset < offset);
        self.free.insert(position, FreeRange { offset, size });
        self.live = self.live.saturating_sub(size);
        // 从插入点左侧开始扫一遍，把相邻（或重叠）的区间合起来：一次线性扫描，不必分别为
        // 前驱 / 后继写两套合并逻辑（那样容易在两个方向同时可合并时漏掉一次）。
        let mut index = position.saturating_sub(1);
        while index + 1 < self.free.len() {
            let current = self.free[index];
            let next = self.free[index + 1];
            if current.offset + current.size >= next.offset {
                self.free[index] = FreeRange {
                    offset: current.offset,
                    size: (next.offset + next.size).saturating_sub(current.offset),
                };
                self.free.remove(index + 1);
            } else {
                index += 1;
            }
        }
        true
    }
}

/// 一次子分配的结果（相对块起点的 offset + 块句柄）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Suballocation {
    pub memory: vk::DeviceMemory,
    pub offset: u64,
    pub size: u64,
}

/// 两级子分配器：`(memory type, kind)` → 块列表；块内是空闲表。
///
/// 线程模型：由 `SharedGpu` 的 `Mutex` 保护（多窗口的 GPU 线程都会申请/归还）。
pub(crate) struct MemoryArena {
    blocks: Vec<ArenaBlock>,
    /// 累计 `vkAllocateMemory` 次数（诊断：R11-6 的「分配次数无上界」）。
    device_allocations: u64,
}

struct ArenaBlock {
    memory_type: u32,
    block: Block,
}

impl MemoryArena {
    pub(crate) const fn new() -> Self {
        Self {
            blocks: Vec::new(),
            device_allocations: 0,
        }
    }

    /// 累计 `vkAllocateMemory` 次数（测试：R11-6 的「分配次数无上界」）。
    #[cfg(test)]
    pub(crate) const fn device_allocations(&self) -> u64 {
        self.device_allocations
    }

    /// 已承诺的字节数（所有块的 `size` 之和）。
    #[cfg(test)]
    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.blocks.iter().map(|entry| entry.block.size).sum()
    }

    /// 正在被资源占用的字节数（所有块的 `live` 之和）。
    #[cfg(test)]
    pub(crate) fn live_bytes(&self) -> u64 {
        self.blocks.iter().map(|entry| entry.block.live).sum()
    }

    /// 申请 `size` 字节（按 `align` 对齐）的子分配。
    ///
    /// 优先复用同 memory type 的块；没有可用空间时才 `vkAllocateMemory` 建新块。
    /// 返回 `(子分配, 主机写入指针)` —— 后者只有 `HOST_VISIBLE` 块才有。
    ///
    /// # Errors
    /// `memoryTypeBits` 没有任何可用类型，或 `vkAllocateMemory` / `vkMapMemory` 失败。
    pub(crate) fn allocate(
        &mut self,
        device: &ash::Device,
        memory_types: &MemoryTypeFlags,
        type_bits: u32,
        need: MemoryNeed,
        size: u64,
        align: u64,
    ) -> Result<(Suballocation, Option<*mut u8>), EngineError> {
        let size = size.max(1);
        let memory_type = pick_memory_type(type_bits, memory_types, need).ok_or_else(|| {
            init_error(
                "pick_memory_type",
                smol_str::format_smolstr!(
                    "no memory type matches bits {type_bits:#x} for {need:?}"
                ),
            )
        })?;
        // ① 复用已有块。
        for entry in &mut self.blocks {
            if entry.memory_type != memory_type {
                continue;
            }
            if let Some(offset) = entry.block.take(size, align) {
                return Ok((
                    Suballocation {
                        memory: entry.block.memory,
                        offset,
                        size,
                    },
                    mapped_pointer(entry.block.mapped, offset),
                ));
            }
        }
        // ② 建新块：尺寸向上取到 2 的幂（至少 `MIN_BLOCK_BYTES`），一次分配容纳后续若干子分配。
        let block_size = size
            .max(MIN_BLOCK_BYTES)
            .checked_next_power_of_two()
            .ok_or_else(|| init_error("allocate_memory(arena)", "block size overflow"))?;
        // SAFETY: 分配参数来自本模块的记账；句柄由 `Block` 独占，`destroy` 时归还。
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(block_size)
                    .memory_type_index(memory_type),
                None,
            )
        }
        .map_err(|error| init_error("allocate_memory(arena)", format!("{error:?}")))?;
        // 主机可见的块整块映射一次（子分配不重复 map/unmap）。
        let mut mapped = 0usize;
        if need.host_visible {
            // SAFETY: 整块分配、`WHOLE_SIZE` 映射；失败时立刻释放内存。
            match unsafe {
                device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            } {
                Ok(pointer) => mapped = pointer as usize,
                Err(error) => {
                    // SAFETY: 内存尚未交给任何人。
                    unsafe { device.free_memory(memory, None) };
                    return Err(init_error("map_memory(arena)", format!("{error:?}")));
                }
            }
        }
        let mut block = Block {
            memory,
            size: block_size,
            mapped,
            free: vec![FreeRange {
                offset: 0,
                size: block_size,
            }],
            live: 0,
        };
        let offset = block
            .take(size, align)
            .expect("a fresh block must fit its first allocation");
        let pointer = mapped_pointer(block.mapped, offset);
        self.blocks.push(ArenaBlock { memory_type, block });
        self.device_allocations += 1;
        Ok((
            Suballocation {
                memory,
                offset,
                size,
            },
            pointer,
        ))
    }

    /// 归还一个子分配（块内合并空闲区间；块永不提前释放 —— 复用它比反复
    /// `vkAllocateMemory`/`vkFreeMemory` 便宜，而几何条目会反复换代）。
    ///
    /// 返回 `false` = 块句柄未知，或该区间与空闲表重叠（**重复释放**）：两种情况下空闲表都不变，
    /// 同一段字节不会被交给两个使用者。
    pub(crate) fn free(&mut self, allocation: Suballocation) -> bool {
        let Some(entry) = self
            .blocks
            .iter_mut()
            .find(|entry| entry.block.memory == allocation.memory)
        else {
            return false;
        };
        entry.block.give_back(allocation.offset, allocation.size)
    }

    /// teardown：销毁全部块（调用方保证队列已空闲，没有在飞命令引用它们）。
    ///
    /// # Safety
    /// 调用方保证 GPU 不再访问这些内存，且所有子分配资源已销毁。
    pub(crate) unsafe fn destroy(&mut self, device: &ash::Device) {
        for entry in self.blocks.drain(..) {
            if entry.block.mapped != 0 {
                // SAFETY: 映射由本模块在 `allocate` 里建立。
                unsafe { device.unmap_memory(entry.block.memory) };
            }
            // SAFETY: 块由本模块分配；调用方保证没有在飞引用。
            unsafe { device.free_memory(entry.block.memory, None) };
        }
    }

    /// 测试用：块数。
    #[cfg(test)]
    pub(crate) fn block_count(&self) -> usize {
        self.blocks.len()
    }
}

impl Default for MemoryArena {
    fn default() -> Self {
        Self::new()
    }
}

/// 由块基址与 offset 得到主机写入指针（`0` 基址 = 未映射）。
fn mapped_pointer(base: usize, offset: u64) -> Option<*mut u8> {
    (base != 0).then(|| base.wrapping_add(offset as usize) as *mut u8)
}

/// 向上对齐（`align` 必须是 2 的幂；Vulkan 的 `VkMemoryRequirements::alignment` 保证如此）。
fn align_up(value: u64, align: u64) -> Option<u64> {
    debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
    let mask = align - 1;
    value.checked_add(mask).map(|value| value & !mask)
}

#[cfg(test)]
mod tests {
    use ash::vk;

    use super::{FreeRange, MIN_BLOCK_BYTES, MemoryArena, MemoryNeed, align_up, pick_memory_type};

    const DEVICE_LOCAL: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::DEVICE_LOCAL;
    const HOST_VISIBLE: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::HOST_VISIBLE;
    const HOST_COHERENT: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::HOST_COHERENT;
    use super::{LAZILY_ALLOCATED, PROTECTED};

    fn types(list: &[vk::MemoryPropertyFlags]) -> Vec<vk::MemoryPropertyFlags> {
        list.to_vec()
    }

    /// 独显（分离 heap）：0 = 纯设备本地、1 = 纯主机可见、2 = BAR（设备本地 + 主机可见）。
    ///
    /// 旧实现（「第一个满足 `HOST_VISIBLE` 的类型」）会选 1（系统内存）⇒ 顶点走 PCIe。
    #[test]
    fn a_discrete_gpu_prefers_device_local_bar_memory_for_geometry() {
        let memory_types = types(&[
            DEVICE_LOCAL,
            HOST_VISIBLE | HOST_COHERENT,
            DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT,
        ]);
        assert_eq!(
            pick_memory_type(0b111, &memory_types, MemoryNeed::HOST_WRITABLE),
            Some(2),
            "BAR（设备本地 + 主机可见）必须胜过纯主机堆"
        );
        assert_eq!(
            pick_memory_type(0b111, &memory_types, MemoryNeed::DEVICE_IMAGE),
            Some(0),
            "纯设备资源落在 BAR 上会白白占用紧张的 BAR 窗口"
        );
        assert_eq!(
            pick_memory_type(0b111, &memory_types, MemoryNeed::READBACK),
            Some(1),
            "读回不需要设备本地"
        );
    }

    /// 没有 BAR 的独显：几何只能落主机堆（仍然必须 `HOST_VISIBLE|HOST_COHERENT`）。
    #[test]
    fn a_discrete_gpu_without_bar_falls_back_to_host_visible() {
        let memory_types = types(&[DEVICE_LOCAL, HOST_VISIBLE | HOST_COHERENT]);
        assert_eq!(
            pick_memory_type(0b11, &memory_types, MemoryNeed::HOST_WRITABLE),
            Some(1)
        );
        assert_eq!(
            pick_memory_type(0b11, &memory_types, MemoryNeed::DEVICE_IMAGE),
            Some(0)
        );
    }

    /// 集显（UMA）：0 = 设备本地 + 主机可见 + 一致 ⇒ 几何与图像同堆。
    #[test]
    fn an_integrated_gpu_uses_the_unified_heap() {
        let memory_types = types(&[
            DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT,
            HOST_VISIBLE | HOST_COHERENT,
        ]);
        assert_eq!(
            pick_memory_type(0b11, &memory_types, MemoryNeed::HOST_WRITABLE),
            Some(0)
        );
        assert_eq!(
            pick_memory_type(0b11, &memory_types, MemoryNeed::DEVICE_IMAGE),
            Some(0)
        );
    }

    /// `memoryTypeBits` 是**硬约束**：资源不允许的类型不参与打分（哪怕偏好更强）。
    #[test]
    fn memory_type_bits_are_respected() {
        let memory_types = types(&[DEVICE_LOCAL, HOST_VISIBLE | HOST_COHERENT]);
        assert_eq!(
            pick_memory_type(0b10, &memory_types, MemoryNeed::HOST_WRITABLE),
            Some(1)
        );
        assert_eq!(
            pick_memory_type(0b100, &memory_types, MemoryNeed::HOST_WRITABLE),
            None
        );
    }

    /// 懒分配与保护内存永远不被选中（旧实现完全没有排除它们）。
    #[test]
    fn lazily_allocated_and_protected_types_are_excluded() {
        let memory_types = types(&[LAZILY_ALLOCATED, PROTECTED | DEVICE_LOCAL, DEVICE_LOCAL]);
        assert_eq!(
            pick_memory_type(0b111, &memory_types, MemoryNeed::DEVICE_IMAGE),
            Some(2),
            "只有普通设备本地类型可用"
        );
        // 只给懒分配类型时没有候选（调用方必须报错，而不是拿到一个用不了的类型）。
        assert_eq!(
            pick_memory_type(0b1, &memory_types, MemoryNeed::DEVICE_IMAGE),
            None
        );
    }

    /// 主机一致是硬需求：没有一致类型时宁可失败（本引擎没有 flush 路径）。
    #[test]
    fn host_coherent_is_a_hard_requirement_for_host_writes() {
        let memory_types = types(&[HOST_VISIBLE, HOST_VISIBLE | HOST_COHERENT]);
        assert_eq!(
            pick_memory_type(0b11, &memory_types, MemoryNeed::HOST_WRITABLE),
            Some(1),
            "非一致类型不得被选中（写入会静默丢失）"
        );
        assert_eq!(
            pick_memory_type(0b1, &memory_types, MemoryNeed::HOST_WRITABLE),
            None,
            "只有非一致类型时必须失败"
        );
    }

    #[test]
    fn alignment_rounds_up_and_detects_overflow() {
        assert_eq!(align_up(0, 256), Some(0));
        assert_eq!(align_up(1, 256), Some(256));
        assert_eq!(align_up(256, 256), Some(256));
        assert_eq!(align_up(257, 256), Some(512));
        assert_eq!(align_up(u64::MAX, 256), None);
    }

    fn block(size: u64) -> super::Block {
        super::Block {
            memory: vk::DeviceMemory::null(),
            size,
            mapped: 0,
            free: vec![FreeRange { offset: 0, size }],
            live: 0,
        }
    }

    /// 块内空闲表的切分与合并：对齐填充与尾部剩余都必须回到空闲表，归还时相邻区间合并。
    #[test]
    fn the_block_free_list_splits_and_merges() {
        let mut block = block(1024);
        assert_eq!(block.take(100, 64), Some(0));
        assert_eq!(block.take(100, 128), Some(128), "对齐填充回到空闲表");
        assert_eq!(block.take(1, 1), Some(100));
        assert_eq!(block.live, 201);
        assert_eq!(
            block.free,
            vec![
                FreeRange {
                    offset: 101,
                    size: 27
                },
                FreeRange {
                    offset: 228,
                    size: 796
                },
            ]
        );
        // 归还中间那块：与左侧剩余、右侧填充、右侧剩余全部合并成一段。
        block.give_back(100, 1);
        assert_eq!(block.live, 200);
        assert_eq!(
            block.free,
            vec![
                FreeRange {
                    offset: 100,
                    size: 28
                },
                FreeRange {
                    offset: 228,
                    size: 796
                },
            ]
        );
        // 再归还前两块：整块合并成一个空闲区间。
        block.give_back(0, 100);
        block.give_back(128, 100);
        assert_eq!(
            block.free,
            vec![FreeRange {
                offset: 0,
                size: 1024
            }]
        );
        assert_eq!(block.live, 0);
        // 只有整个块空闲时才能满足超大请求；切满之后必须明确失败（调用方建新块）。
        assert_eq!(block.take(1024, 1), Some(0));
        assert_eq!(block.take(1, 1), None);
    }

    /// 对齐填充超出剩余空间时不得越界切分。
    #[test]
    fn a_too_small_aligned_hole_is_skipped() {
        let mut block = block(64);
        assert_eq!(block.take(60, 1), Some(0));
        // 只剩 4 字节，且需要 32 对齐 ⇒ 对齐后的 32 超出剩余空间。
        assert_eq!(block.take(1, 32), None);
        assert_eq!(block.live, 60);
    }

    /// 子分配器的记账：`reserved` / `live` / 块数（Vulkan 分配路径在 lavapipe 上测）。
    #[test]
    fn the_arena_reports_bytes_and_blocks() {
        let mut arena = MemoryArena::new();
        assert_eq!(arena.device_allocations(), 0);
        assert_eq!(arena.reserved_bytes(), 0);
        assert_eq!(arena.live_bytes(), 0);
        assert_eq!(arena.block_count(), 0);
        arena.blocks.push(super::ArenaBlock {
            memory_type: 0,
            block: block(MIN_BLOCK_BYTES),
        });
        assert_eq!(arena.reserved_bytes(), MIN_BLOCK_BYTES);
        assert_eq!(arena.live_bytes(), 0);
        assert_eq!(arena.block_count(), 1);
    }

    /// 归还到未知块：明确失败（陈旧句柄不得被当成别的块）。
    #[test]
    fn freeing_into_an_unknown_block_is_rejected() {
        let mut arena = MemoryArena::new();
        assert!(
            !arena.free(super::Suballocation {
                memory: vk::DeviceMemory::null(),
                offset: 0,
                size: 16,
            }),
            "未知块必须拒绝归还"
        );
    }

    /// AR3 §5.5 / R11-6 的 P2：**重复释放**（或伪造区间）必须被拒绝 —— 否则同一段字节会同时
    /// 出现在空闲表里两次，随后被两次 `take` 交给两个使用者（同一区间两个写入者 = 数据错乱）。
    #[test]
    fn a_double_free_is_rejected_instead_of_handing_the_same_bytes_to_two_owners() {
        let mut block = block(256);
        assert_eq!(block.take(64, 1), Some(0));
        assert!(block.give_back(0, 64), "第一次归还是合法的");
        assert_eq!(block.live, 0);
        // 同一区间再归还一次：必须被拒绝，且空闲表与记账都不变。
        assert!(!block.give_back(0, 64), "重复归还必须被拒绝");
        assert_eq!(block.live, 0, "被拒绝的归还不改变记账");
        assert_eq!(
            block.free,
            vec![FreeRange {
                offset: 0,
                size: 256
            }]
        );
        // 部分重叠（伪造区间）同样拒绝：它一半是已分配字节、一半已经在空闲表里。
        assert_eq!(block.take(32, 1), Some(0));
        assert!(!block.give_back(16, 32), "与空闲区间重叠的归还必须被拒绝");
        assert_eq!(
            block.free,
            vec![FreeRange {
                offset: 32,
                size: 224
            }]
        );
        // 合法归还仍然成功（这道检查不能把正常路径也挡掉）。
        assert!(block.give_back(0, 32));
        assert_eq!(
            block.free,
            vec![FreeRange {
                offset: 0,
                size: 256
            }]
        );
    }
}
