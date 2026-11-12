//! 命令队列 / 每 in-flight 槽的 allocator + list / fence 等待 / retire 排空
//! （`design/phase-08-d3d12.md` §5.2）。
//!
//! 本阶段**同步提交**：`submit` 在 `Signal` 之后立刻等到 fence 到达。ring 结构仍按
//! `MAX_FRAMES_IN_FLIGHT` 组织（P09 异步化不改结构）。

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Graphics::Direct3D12::*,
        System::Threading::{CreateEventW, INFINITE, WaitForSingleObject},
    },
    core::Interface,
};

use crate::{
    error::EngineError,
    render::{
        frame::{FrameSlot, InFlightRing, MAX_FRAMES_IN_FLIGHT},
        retire::{GpuResourceHandle, RetireQueue},
        windows::device::{Device, init_error, init_error_from},
    },
    window::FrameIndex,
};

/// 直接队列 + 每槽 allocator/list + fence。
pub(crate) struct Queue {
    raw: ID3D12CommandQueue,
    allocators: Vec<ID3D12CommandAllocator>,
    lists: Vec<ID3D12GraphicsCommandList>,
    fence: ID3D12Fence,
    event: HANDLE,
    ring: InFlightRing,
    retire: RetireQueue,
}

impl Drop for Queue {
    fn drop(&mut self) {
        // SAFETY: `event` 由本结构独占；`Drop` 只执行一次，关闭后不再使用。
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

impl Queue {
    pub(crate) fn new(device: &Device) -> Result<Self, EngineError> {
        let raw: ID3D12CommandQueue = unsafe {
            device.raw().CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                Priority: 0,
                Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
                NodeMask: 0,
            })
        }
        .map_err(|error| init_error_from("CreateCommandQueue", error))?;

        let mut allocators = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        let mut lists = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let allocator: ID3D12CommandAllocator = unsafe {
                device
                    .raw()
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            }
            .map_err(|error| init_error_from("CreateCommandAllocator", error))?;
            let list: ID3D12GraphicsCommandList = unsafe {
                device
                    .raw()
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            }
            .map_err(|error| init_error_from("CreateCommandList", error))?;
            unsafe { list.Close() }.map_err(|error| init_error_from("Close", error))?;
            allocators.push(allocator);
            lists.push(list);
        }

        let fence: ID3D12Fence = unsafe { device.raw().CreateFence(0, D3D12_FENCE_FLAG_NONE) }
            .map_err(|error| init_error_from("CreateFence", error))?;
        // `CreateEventW` 需要 `Win32_Security` feature（DEVIATIONS F-79）。
        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| init_error_from("CreateEventW", error))?;

        Ok(Self {
            raw,
            allocators,
            lists,
            fence,
            event,
            ring: InFlightRing::new(),
            retire: RetireQueue::new(),
        })
    }

    /// 等待该槽的 fence → 复位 allocator/list。返回本帧的槽位与 fence 值。
    pub(crate) fn begin(&mut self, index: FrameIndex) -> Result<FrameSlot, EngineError> {
        let slot = self.ring.acquire(index);
        self.wait_for(slot.wait_value)?;
        let allocator = &self.allocators[slot.index];
        unsafe { allocator.Reset() }
            .map_err(|error| init_error_from("CommandAllocator::Reset", error))?;
        let list = &self.lists[slot.index];
        unsafe { list.Reset(allocator, None) }
            .map_err(|error| init_error_from("CommandList::Reset", error))?;
        Ok(slot)
    }

    /// 正在录制的命令列表（`begin` 之后有效）。
    pub(crate) fn list(&self, slot: FrameSlot) -> &ID3D12GraphicsCommandList {
        &self.lists[slot.index]
    }

    /// 关闭 → 执行 → `Signal` → 同步等待到达。
    pub(crate) fn submit(&mut self, slot: FrameSlot) -> Result<(), EngineError> {
        let list = &self.lists[slot.index];
        unsafe { list.Close() }.map_err(|error| init_error_from("CommandList::Close", error))?;
        let command_list: ID3D12CommandList = list
            .cast()
            .map_err(|error| init_error_from("cast ID3D12CommandList", error))?;
        unsafe { self.raw.ExecuteCommandLists(&[Some(command_list)]) };
        unsafe { self.raw.Signal(&self.fence, slot.signal_value) }
            .map_err(|error| init_error_from("CommandQueue::Signal", error))?;
        self.wait_for(slot.signal_value)?;
        self.ring.advance(slot);
        Ok(())
    }

    /// 等到最后一次提交完成（resize / 快照 / 退出）。
    pub(crate) fn wait_idle(&mut self) -> Result<(), EngineError> {
        self.wait_for(self.ring.last_signaled())
    }

    /// 复位**所有** in-flight 槽的 allocator + list 并立即关闭列表（resize 前必须调用）。
    ///
    /// 关闭的 command list 仍记录着它引用过的资源；不 Reset 的话，swapchain 的 back buffer
    /// 会一直有引用，`ResizeBuffers` 返回 `DXGI_ERROR_INVALID_CALL`（DEVIATIONS F-86）。
    /// 调用方必须先 `wait_idle`（allocator 在 GPU 未完成时不可 Reset）。
    pub(crate) fn reset_in_flight_lists(&mut self) -> Result<(), EngineError> {
        for index in 0..self.allocators.len() {
            let allocator = &self.allocators[index];
            // SAFETY: 上一个提交已由 `wait_idle` 等到完成，allocator 不再被 GPU 使用。
            unsafe { allocator.Reset() }
                .map_err(|error| init_error_from("CommandAllocator::Reset", error))?;
            let list = &self.lists[index];
            // SAFETY: 列表已关闭（`submit` 或上一次本调用），重开后立刻关闭。
            unsafe { list.Reset(allocator, None) }
                .map_err(|error| init_error_from("CommandList::Reset", error))?;
            // SAFETY: 空列表；关闭之后再执行才是合法的，这里只需要它丢掉记录的资源引用。
            unsafe { list.Close() }.map_err(|error| init_error_from("Close", error))?;
        }
        Ok(())
    }

    /// 帧外的一次性提交（readback / resize）：等 idle → 用槽 0 录制 → 提交 → 等完成。
    /// 不推进 ring 的槽位（只预留一个 fence 值），调用前后都必须 idle。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn submit_immediate(
        &mut self,
        record: impl FnOnce(&ID3D12GraphicsCommandList) -> Result<(), EngineError>,
    ) -> Result<(), EngineError> {
        self.wait_idle()?;
        let allocator = &self.allocators[0];
        unsafe { allocator.Reset() }
            .map_err(|error| init_error_from("CommandAllocator::Reset", error))?;
        let list = &self.lists[0];
        unsafe { list.Reset(allocator, None) }
            .map_err(|error| init_error_from("CommandList::Reset", error))?;
        record(list)?;
        unsafe { list.Close() }.map_err(|error| init_error_from("CommandList::Close", error))?;
        let command_list: ID3D12CommandList = list
            .cast()
            .map_err(|error| init_error_from("cast ID3D12CommandList", error))?;
        unsafe { self.raw.ExecuteCommandLists(&[Some(command_list)]) };
        let value = self.ring.reserve();
        unsafe { self.raw.Signal(&self.fence, value) }
            .map_err(|error| init_error_from("CommandQueue::Signal", error))?;
        self.wait_for(value)
    }

    /// 延迟销毁：本帧用过的资源至少要活到 `index` 的 fence 完成。
    pub(crate) fn retire(&mut self, index: FrameIndex, handle: GpuResourceHandle) {
        self.retire.retire(index, handle);
    }

    /// 把 `completed` 及更早的句柄交回 `device`（真正 drop COM 对象）。返回释放数。
    pub(crate) fn release_completed(
        &mut self,
        completed: FrameIndex,
        device: &mut Device,
    ) -> usize {
        self.retire.on_completed(completed, &mut |handle| {
            device.release(handle);
        })
    }

    /// 尚未释放的句柄数（诊断 / 测试）。
    #[cfg(test)]
    pub(crate) fn pending_retire(&self) -> usize {
        self.retire.pending()
    }

    pub(crate) fn timestamp_frequency(&self) -> Result<u64, EngineError> {
        unsafe { self.raw.GetTimestampFrequency() }
            .map_err(|error| init_error_from("GetTimestampFrequency", error))
    }

    pub(crate) fn raw(&self) -> &ID3D12CommandQueue {
        &self.raw
    }

    fn wait_for(&self, value: u64) -> Result<(), EngineError> {
        if value == 0 || unsafe { self.fence.GetCompletedValue() } >= value {
            return Ok(());
        }
        unsafe { self.fence.SetEventOnCompletion(value, self.event) }
            .map_err(|error| init_error_from("SetEventOnCompletion", error))?;
        let wait = unsafe { WaitForSingleObject(self.event, INFINITE) };
        if wait == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(init_error(
                "WaitForSingleObject(fence)",
                windows::core::HRESULT::from_win32(wait.0),
            ))
        }
    }
}

/// 打开 D3D12 调试层（**仅测试**；本机常缺 SDK 组件，失败静默）。
#[cfg(test)]
pub(crate) fn enable_debug_layer_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static TOUCHED: AtomicBool = AtomicBool::new(false);
    if TOUCHED.swap(true, Ordering::Relaxed) {
        return;
    }
    let mut debug: Option<ID3D12Debug> = None;
    if unsafe { D3D12GetDebugInterface(&mut debug) }.is_ok()
        && let Some(debug) = debug
    {
        unsafe { debug.EnableDebugLayer() };
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::Queue;
    use crate::{render::windows::device::Device, window::FrameIndex};

    fn index(value: u32) -> FrameIndex {
        FrameIndex::from_raw(value)
    }

    #[test]
    fn submits_and_advances_the_fence_for_many_frames() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        super::enable_debug_layer_once();
        let device = Device::new_warp().expect("WARP device");
        let mut queue = Queue::new(&device).expect("queue");
        for value in 1..=7u32 {
            let slot = queue.begin(index(value)).expect("begin");
            assert_eq!(slot.index, (value as usize) % 3);
            queue.submit(slot).expect("submit");
        }
        queue.wait_idle().expect("idle");
        assert_eq!(queue.pending_retire(), 0);
    }
}
