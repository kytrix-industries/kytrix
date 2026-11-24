use ash::vk;

use crate::{error::EngineError, render::linux::device::init_error};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryNeed {
    pub host_visible: bool,
    pub host_coherent: bool,
    pub device_local: bool,
}

impl MemoryNeed {
    pub(crate) const HOST_WRITABLE: Self = Self {
        host_visible: true,
        host_coherent: true,
        device_local: true,
    };

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) const READBACK: Self = Self {
        host_visible: true,
        host_coherent: true,
        device_local: false,
    };

    pub(crate) const DEVICE_IMAGE: Self = Self {
        host_visible: false,
        host_coherent: false,
        device_local: true,
    };
}

pub(crate) type MemoryTypeFlags = [vk::MemoryPropertyFlags];

const LAZILY_ALLOCATED: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::LAZILY_ALLOCATED;
const PROTECTED: vk::MemoryPropertyFlags = vk::MemoryPropertyFlags::PROTECTED;

pub(crate) fn pick_memory_type(
    type_bits: u32,
    types: &MemoryTypeFlags,
    need: MemoryNeed,
) -> Option<u32> {
    let mut best: Option<(i32, u32)> = None;
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
                score += 4;
            } else {
                score -= 2;
            }
        }
        if !need.host_visible && flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
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

const MIN_BLOCK_BYTES: u64 = 1 << 20;

struct Block {
    memory: vk::DeviceMemory,
    size: u64,
    mapped: usize,
    free: Vec<FreeRange>,
    live: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FreeRange {
    offset: u64,
    size: u64,
}

impl Block {
    fn take(&mut self, size: u64, align: u64) -> Option<u64> {
        let align = align.max(1);
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

    fn give_back(&mut self, offset: u64, size: u64) -> bool {
        debug_assert!(size > 0, "a zero-sized suballocation is never handed out");
        debug_assert!(
            offset + size <= self.size,
            "suballocation {offset}+{size} exceeds block {}",
            self.size
        );
        let end = offset + size;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Suballocation {
    pub memory: vk::DeviceMemory,
    pub offset: u64,
    pub size: u64,
}

pub(crate) struct MemoryArena {
    blocks: Vec<ArenaBlock>,
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

    #[cfg(test)]
    pub(crate) const fn device_allocations(&self) -> u64 {
        self.device_allocations
    }

    #[cfg(test)]
    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.blocks.iter().map(|entry| entry.block.size).sum()
    }

    #[cfg(test)]
    pub(crate) fn live_bytes(&self) -> u64 {
        self.blocks.iter().map(|entry| entry.block.live).sum()
    }

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
        let block_size = size
            .max(MIN_BLOCK_BYTES)
            .checked_next_power_of_two()
            .ok_or_else(|| init_error("allocate_memory(arena)", "block size overflow"))?;
        let memory = unsafe {
            device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(block_size)
                    .memory_type_index(memory_type),
                None,
            )
        }
        .map_err(|error| init_error("allocate_memory(arena)", format!("{error:?}")))?;
        let mut mapped = 0usize;
        if need.host_visible {
            match unsafe {
                device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            } {
                Ok(pointer) => mapped = pointer as usize,
                Err(error) => {
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

    pub(crate) unsafe fn destroy(&mut self, device: &ash::Device) {
        for entry in self.blocks.drain(..) {
            if entry.block.mapped != 0 {
                unsafe { device.unmap_memory(entry.block.memory) };
            }
            unsafe { device.free_memory(entry.block.memory, None) };
        }
    }

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

fn mapped_pointer(base: usize, offset: u64) -> Option<*mut u8> {
    (base != 0).then(|| base.wrapping_add(offset as usize) as *mut u8)
}

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

    #[test]
    fn lazily_allocated_and_protected_types_are_excluded() {
        let memory_types = types(&[LAZILY_ALLOCATED, PROTECTED | DEVICE_LOCAL, DEVICE_LOCAL]);
        assert_eq!(
            pick_memory_type(0b111, &memory_types, MemoryNeed::DEVICE_IMAGE),
            Some(2),
            "只有普通设备本地类型可用"
        );
        assert_eq!(
            pick_memory_type(0b1, &memory_types, MemoryNeed::DEVICE_IMAGE),
            None
        );
    }

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
        assert_eq!(block.take(1024, 1), Some(0));
        assert_eq!(block.take(1, 1), None);
    }

    #[test]
    fn a_too_small_aligned_hole_is_skipped() {
        let mut block = block(64);
        assert_eq!(block.take(60, 1), Some(0));
        assert_eq!(block.take(1, 32), None);
        assert_eq!(block.live, 60);
    }

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

    #[test]
    fn a_double_free_is_rejected_instead_of_handing_the_same_bytes_to_two_owners() {
        let mut block = block(256);
        assert_eq!(block.take(64, 1), Some(0));
        assert!(block.give_back(0, 64), "第一次归还是合法的");
        assert_eq!(block.live, 0);
        assert!(!block.give_back(0, 64), "重复归还必须被拒绝");
        assert_eq!(block.live, 0, "被拒绝的归还不改变记账");
        assert_eq!(
            block.free,
            vec![FreeRange {
                offset: 0,
                size: 256
            }]
        );
        assert_eq!(block.take(32, 1), Some(0));
        assert!(!block.give_back(16, 32), "与空闲区间重叠的归还必须被拒绝");
        assert_eq!(
            block.free,
            vec![FreeRange {
                offset: 32,
                size: 224
            }]
        );
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
