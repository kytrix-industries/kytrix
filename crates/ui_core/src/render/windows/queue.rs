use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Graphics::Direct3D12::*,
        System::Threading::{CreateEventW, WaitForSingleObject},
    },
    core::Interface,
};

use super::{
    diagnostics,
    hresult::{is_device_removal, removal_reason_label},
};
use crate::{
    error::EngineError,
    render::{
        frame::{FrameSlot, InFlightRing, MAX_FRAMES_IN_FLIGHT},
        retire::{GpuResourceHandle, RetireQueue},
        windows::device::{SharedGpu, init_error, init_error_from},
    },
};

pub(crate) const FENCE_TIMEOUT_MS: u32 = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    Signaled,
    TimedOut,
}

impl WaitOutcome {
    pub(crate) fn is_signaled(self) -> bool {
        matches!(self, Self::Signaled)
    }
}

pub(crate) struct StreamWatermark {
    fence: ID3D12Fence,
    value: AtomicU64,
    retired: [AtomicU64; 2],
}

impl StreamWatermark {
    pub(crate) fn new(fence: ID3D12Fence) -> Self {
        Self {
            fence,
            value: AtomicU64::new(0),
            retired: [AtomicU64::new(0), AtomicU64::new(0)],
        }
    }

    pub(crate) fn retire_generation(&self, generation: u8, value: u64) {
        debug_assert!(
            (generation as usize) < self.retired.len(),
            "atlas generation out of range"
        );
        self.retired[generation as usize].store(value, Ordering::Release);
    }

    pub(crate) fn generation_idle(&self, generation: u8) -> bool {
        let retired = match self.retired.get(generation as usize) {
            Some(retired) => retired.load(Ordering::Acquire),
            None => return true,
        };
        if retired == 0 {
            return true;
        }
        (unsafe { self.fence.GetCompletedValue() }) >= retired
    }

    fn publish(&self, value: u64) {
        self.value.store(value, Ordering::Release);
    }

    pub(crate) fn value(&self) -> u64 {
        self.value.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn publish_for_test(&self, value: u64) {
        self.publish(value);
    }
}

pub(crate) struct CommandStream {
    shared: Arc<SharedGpu>,
    allocators: Vec<ID3D12CommandAllocator>,
    lists: Vec<ID3D12GraphicsCommandList>,
    fence: ID3D12Fence,
    event: HANDLE,
    ring: InFlightRing,
    submitted: VecDeque<u64>,
    retire: RetireQueue,
    last_retire_sequence: Option<u64>,
    watermark: Arc<StreamWatermark>,
}

impl Drop for CommandStream {
    fn drop(&mut self) {
        self.shared.unregister_watermark(&self.watermark);
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

impl CommandStream {
    pub(crate) fn new(shared: &Arc<SharedGpu>) -> Result<Self, EngineError> {
        let mut allocators = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        let mut lists = Vec::with_capacity(MAX_FRAMES_IN_FLIGHT);
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let allocator: ID3D12CommandAllocator = unsafe {
                shared
                    .raw()
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            }
            .map_err(|error| init_error_from("CreateCommandAllocator", error))?;
            let list: ID3D12GraphicsCommandList = unsafe {
                shared
                    .raw()
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            }
            .map_err(|error| init_error_from("CreateCommandList", error))?;
            unsafe { list.Close() }.map_err(|error| init_error_from("Close", error))?;
            allocators.push(allocator);
            lists.push(list);
        }

        let fence: ID3D12Fence = unsafe { shared.raw().CreateFence(0, D3D12_FENCE_FLAG_NONE) }
            .map_err(|error| init_error_from("CreateFence", error))?;
        let event = unsafe { CreateEventW(None, false, false, None) }
            .map_err(|error| init_error_from("CreateEventW", error))?;

        let watermark = shared.register_watermark(fence.clone());
        Ok(Self {
            shared: Arc::clone(shared),
            allocators,
            lists,
            fence,
            event,
            ring: InFlightRing::new(),
            submitted: VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT),
            retire: RetireQueue::new(),
            last_retire_sequence: None,
            watermark,
        })
    }

    pub(crate) fn acquire_slot(&mut self) -> FrameSlot {
        self.ring.acquire()
    }

    pub(crate) fn wait_slot_fence(&self, value: u64) -> Result<WaitOutcome, EngineError> {
        self.wait_for(value)
    }

    pub(crate) fn reset_slot(&mut self, slot: FrameSlot) -> Result<(), EngineError> {
        let allocator = &self.allocators[slot.index];
        unsafe { allocator.Reset() }
            .map_err(|error| init_error_from("CommandAllocator::Reset", error))?;
        let list = &self.lists[slot.index];
        unsafe { list.Reset(allocator, None) }
            .map_err(|error| init_error_from("CommandList::Reset", error))?;
        Ok(())
    }

    pub(crate) fn list(&self, slot: FrameSlot) -> &ID3D12GraphicsCommandList {
        &self.lists[slot.index]
    }

    pub(crate) fn submit(&mut self, slot: FrameSlot) -> Result<(), EngineError> {
        let list = &self.lists[slot.index];
        unsafe { list.Close() }.map_err(|error| init_error_from("CommandList::Close", error))?;
        let command_list: ID3D12CommandList = list
            .cast()
            .map_err(|error| init_error_from("cast ID3D12CommandList", error))?;
        unsafe {
            self.shared
                .queue()
                .ExecuteCommandLists(&[Some(command_list)])
        };
        unsafe { self.shared.queue().Signal(&self.fence, slot.signal_value) }
            .map_err(|error| init_error_from("CommandQueue::Signal", error))?;
        self.ring.advance(slot);
        self.submitted.push_back(slot.signal_value);
        self.watermark.publish(slot.signal_value);
        Ok(())
    }

    pub(crate) fn set_completion_event(
        &self,
        value: u64,
        event: HANDLE,
    ) -> Result<(), EngineError> {
        unsafe { self.fence.SetEventOnCompletion(value, event) }
            .map_err(|error| init_error_from("SetEventOnCompletion(completion)", error))
    }

    pub(crate) fn completed_value(&self) -> u64 {
        unsafe { self.fence.GetCompletedValue() }
    }

    pub(crate) fn wait_idle(&mut self) -> Result<WaitOutcome, EngineError> {
        self.wait_for(self.ring.last_signaled())
    }

    pub(crate) fn reset_in_flight_lists(&mut self) -> Result<(), EngineError> {
        for index in 0..self.allocators.len() {
            let allocator = &self.allocators[index];
            unsafe { allocator.Reset() }
                .map_err(|error| init_error_from("CommandAllocator::Reset", error))?;
            let list = &self.lists[index];
            unsafe { list.Reset(allocator, None) }
                .map_err(|error| init_error_from("CommandList::Reset", error))?;
            unsafe { list.Close() }.map_err(|error| init_error_from("Close", error))?;
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn submit_immediate(
        &mut self,
        record: impl FnOnce(&ID3D12GraphicsCommandList) -> Result<(), EngineError>,
    ) -> Result<(), EngineError> {
        if self.wait_idle()? != WaitOutcome::Signaled {
            return Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "submit_immediate requires an idle command stream",
                ),
            });
        }
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
        unsafe {
            self.shared
                .queue()
                .ExecuteCommandLists(&[Some(command_list)])
        };
        let slot = FrameSlot {
            index: 0,
            wait_value: self.ring.last_signaled(),
            signal_value: self.ring.last_signaled() + 1,
        };
        unsafe { self.shared.queue().Signal(&self.fence, slot.signal_value) }
            .map_err(|error| init_error_from("CommandQueue::Signal", error))?;
        self.ring.advance(slot);
        self.watermark.publish(slot.signal_value);
        match self.wait_for(slot.signal_value)? {
            WaitOutcome::Signaled => Ok(()),
            WaitOutcome::TimedOut => Err(EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "submit_immediate timed out while the device is still present",
                ),
            }),
        }
    }

    pub(crate) fn retire(&mut self, sequence: u64, handle: GpuResourceHandle) {
        self.last_retire_sequence = Some(
            self.last_retire_sequence
                .map_or(sequence, |last| last.max(sequence)),
        );
        self.retire.retire(sequence, handle);
    }

    pub(crate) fn drain_retire(&mut self) {
        let Some(last) = self.last_retire_sequence else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        self.retire.on_completed(last, &mut |handle| {
            let _ = shared.release(handle);
        });
        self.submitted.clear();
    }

    pub(crate) fn release_completed(&mut self) -> usize {
        let completed = self.completed_value();
        let mut highest = None;
        while let Some(value) = self.submitted.front() {
            if *value > completed {
                break;
            }
            let value = self.submitted.pop_front().expect("front checked above");
            highest = Some(value);
        }
        match highest {
            Some(sequence) => {
                let shared = Arc::clone(&self.shared);
                self.retire.on_completed(sequence, &mut |handle| {
                    let _ = shared.release(handle);
                })
            }
            None => 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_retire(&self) -> usize {
        self.retire.pending()
    }

    pub(crate) fn timestamp_frequency(&self) -> Result<u64, EngineError> {
        unsafe { self.shared.queue().GetTimestampFrequency() }
            .map_err(|error| init_error_from("GetTimestampFrequency", error))
    }

    fn wait_for(&self, value: u64) -> Result<WaitOutcome, EngineError> {
        if value == 0 || self.completed_value() >= value {
            return Ok(WaitOutcome::Signaled);
        }
        unsafe { self.fence.SetEventOnCompletion(value, self.event) }
            .map_err(|error| init_error_from("SetEventOnCompletion", error))?;
        let wait = unsafe { WaitForSingleObject(self.event, FENCE_TIMEOUT_MS) };
        if wait == WAIT_OBJECT_0 {
            return Ok(WaitOutcome::Signaled);
        }
        if wait.0 != WAIT_TIMEOUT.0 {
            return Err(init_error(
                "WaitForSingleObject(fence)",
                windows::core::HRESULT::from_win32(wait.0),
            ));
        }
        self.classify_timeout()
    }

    fn classify_timeout(&self) -> Result<WaitOutcome, EngineError> {
        let removed = unsafe { self.shared.raw().GetDeviceRemovedReason() };
        match removed {
            Ok(()) => {
                diagnostics::frame_wait_timeout();
                Ok(WaitOutcome::TimedOut)
            }
            Err(error) => {
                let hr = error.code();
                if is_device_removal(hr) {
                    if let Some(sticky) = self.shared.note_failure() {
                        return Err(sticky);
                    }
                    return Err(EngineError::DeviceLost {
                        reason: smol_str::format_smolstr!(
                            "GPU fence wait failed: {:#010X} {} ({})",
                            hr.0 as u32,
                            hr.message(),
                            removal_reason_label(hr)
                        ),
                        log_path: PathBuf::new(),
                    });
                }
                diagnostics::frame_wait_timeout();
                Ok(WaitOutcome::TimedOut)
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{CommandStream, WaitOutcome};
    use crate::render::{retire::GpuResourceHandle, windows::device::SharedGpu};

    #[test]
    fn submits_and_advances_the_fence_for_many_frames() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        super::super::debug::enable_debug_layer();
        let shared = SharedGpu::new_warp().expect("WARP shared gpu");
        let mut stream = CommandStream::new(&shared).expect("command stream");
        for value in 1..=7u32 {
            let slot = stream.acquire_slot();
            assert_eq!(
                slot.index,
                (value as usize - 1) % 3,
                "槽位按提交次数轮转，不从 FrameIndex 取模"
            );
            assert_eq!(
                stream.wait_slot_fence(slot.wait_value).expect("slot fence"),
                WaitOutcome::Signaled
            );
            stream.reset_slot(slot).expect("reset slot");
            stream.submit(slot).expect("submit");
            assert_eq!(
                stream.wait_slot_fence(slot.signal_value).expect("signaled"),
                WaitOutcome::Signaled
            );
        }
        assert_eq!(stream.wait_idle().expect("idle"), WaitOutcome::Signaled);
        assert_eq!(stream.pending_retire(), 0);
    }

    #[test]
    fn the_stream_publishes_its_watermark_on_every_submit() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let shared = SharedGpu::new_warp().expect("WARP shared gpu");
        assert!(shared.watermark_values().is_empty(), "没有窗口时水位表为空");
        let mut stream = CommandStream::new(&shared).expect("command stream");
        assert_eq!(shared.watermark_values(), vec![0], "新流的水位是 0");
        for value in 1..=3u32 {
            let slot = stream.acquire_slot();
            assert_eq!(
                stream.wait_slot_fence(slot.wait_value).expect("slot fence"),
                WaitOutcome::Signaled
            );
            stream.reset_slot(slot).expect("reset");
            stream.submit(slot).expect("submit");
            assert_eq!(
                shared.watermark_values(),
                vec![u64::from(value)],
                "每次提交都必须推进水位"
            );
        }
        let _ = stream.wait_idle().expect("idle");
        drop(stream);
        assert!(
            shared.watermark_values().is_empty(),
            "提交流销毁后水位条目必须注销（不留悬空 Weak）"
        );
    }

    #[test]
    fn retire_keys_are_fence_sequences_beyond_the_frame_index_range() {
        let _guard = crate::render::windows::device::gpu_test_lock();
        let shared = SharedGpu::new_warp().expect("WARP shared gpu");
        let mut stream = CommandStream::new(&shared).expect("command stream");
        let slot = stream.acquire_slot();
        assert_eq!(
            stream.wait_slot_fence(slot.wait_value).expect("slot fence"),
            WaitOutcome::Signaled
        );
        stream.reset_slot(slot).expect("reset slot");
        stream.submit(slot).expect("submit");
        assert_eq!(stream.wait_idle().expect("idle"), WaitOutcome::Signaled);
        let handle = GpuResourceHandle::new(1, 0, u32::MAX);

        stream.retire(slot.signal_value, handle);
        assert_eq!(
            stream.release_completed(),
            1,
            "fence 已到的提交点按 u64 序号释放"
        );
        assert_eq!(stream.pending_retire(), 0);

        let future = u64::from(u32::MAX) + 7;
        stream.retire(future, handle);
        assert_eq!(stream.release_completed(), 0, "fence 没到那个提交点");
        assert_eq!(
            stream.pending_retire(),
            1,
            "键是 u64 提交序号，不会被已完成的更早提交放掉"
        );
        stream.drain_retire();
        assert_eq!(stream.pending_retire(), 0);
    }
}
