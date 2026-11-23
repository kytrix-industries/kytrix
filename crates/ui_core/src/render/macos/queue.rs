use std::{
    collections::VecDeque,
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use block2::RcBlock;
use dispatch2::{DispatchRetained, DispatchSemaphore, DispatchTime};
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandQueue, MTLDevice};

use crate::{
    error::EngineError,
    render::{
        frame::{FrameSlot, InFlightRing, MAX_FRAMES_IN_FLIGHT},
        macos::{
            device::{Device, init_error},
            timer::GpuTimer,
        },
        retire::{GpuResourceHandle, RetireQueue},
        timer::GpuTimerSample,
    },
    window::FrameIndex,
};

pub(crate) struct InFlight {
    count: AtomicUsize,
}

impl InFlight {
    pub(crate) fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    fn note_submitted(self: &Arc<Self>) -> SubmissionTicket {
        let previous = self.count.fetch_add(1, Ordering::AcqRel);
        debug_assert!(
            previous < MAX_FRAMES_IN_FLIGHT * 2,
            "in-flight accounting exceeded its bound: {previous} submissions are still unaccounted for"
        );
        SubmissionTicket {
            ledger: Arc::clone(self),
            committed: false,
        }
    }

    pub(crate) fn note_drained(&self, drained: usize) {
        if drained == 0 {
            return;
        }
        let previous = self.count.fetch_sub(drained, Ordering::AcqRel);
        assert!(
            previous >= drained,
            "in-flight accounting underflow: drained {drained} completions with only {previous} recorded"
        );
    }

    pub(crate) fn get(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn preset(&self, count: usize) {
        self.count.store(count, Ordering::Release);
    }
}

struct SubmissionTicket {
    ledger: Arc<InFlight>,
    committed: bool,
}

impl SubmissionTicket {
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for SubmissionTicket {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.ledger.count.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct Queue {
    raw: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    in_flight: Arc<InFlight>,
    ring: InFlightRing,
    slots: [DispatchRetained<DispatchSemaphore>; MAX_FRAMES_IN_FLIGHT],
    order: CompletionOrder,
    retire: RetireQueue,
    timer: GpuTimer,
}

impl Queue {
    pub(crate) fn new(device: &Device, in_flight: Arc<InFlight>) -> Result<Self, EngineError> {
        let raw = device
            .raw()
            .newCommandQueue()
            .ok_or_else(|| init_error("newCommandQueue", "returned nil"))?;
        Ok(Self {
            raw,
            in_flight,
            ring: InFlightRing::new(),
            slots: std::array::from_fn(|_| DispatchSemaphore::new(1)),
            order: CompletionOrder::new(),
            retire: RetireQueue::new(),
            timer: GpuTimer,
        })
    }

    pub(crate) fn try_acquire(&mut self) -> Option<FrameSlot> {
        let candidate = self.ring.peek();
        if self.slots[candidate.index].wait(DispatchTime::NOW) != 0 {
            return None;
        }
        Some(self.ring.acquire())
    }

    pub(crate) fn release(&self, slot: FrameSlot) {
        self.slots[slot.index].signal();
    }

    pub(crate) fn begin(
        &mut self,
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, EngineError> {
        let buffer = self
            .raw
            .commandBuffer()
            .ok_or_else(|| init_error("commandBuffer", "returned nil"))?;
        Ok(buffer)
    }

    pub(crate) fn submit(
        &mut self,
        index: FrameIndex,
        slot: FrameSlot,
        buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        completed: impl Fn(Retained<ProtocolObject<dyn MTLCommandBuffer>>) + Send + 'static,
    ) -> Result<(), EngineError> {
        let ticket = self.in_flight.note_submitted();
        let permit = self.slots[slot.index].clone();
        let block = RcBlock::new(
            move |buffer: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                permit.signal();
                let buffer =
                    unsafe { Retained::retain(buffer.as_ptr()) }.expect("Metal 的完成回调参数非空");
                completed(buffer);
            },
        );
        unsafe { buffer.addCompletedHandler(RcBlock::as_ptr(&block)) };
        buffer.commit();
        self.ring.advance(slot);
        self.order.submit(index, slot.signal_value);
        debug_assert_eq!(
            self.ring.last_signaled(),
            slot.signal_value,
            "ring must advance exactly one step per submitted frame"
        );
        ticket.commit();
        Ok(())
    }

    pub(crate) fn submit_blocking(
        &mut self,
        index: FrameIndex,
        slot: FrameSlot,
        buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        device: &mut Device,
    ) -> Result<Option<GpuTimerSample>, EngineError> {
        let ticket = self.in_flight.note_submitted();
        buffer.commit();
        self.ring.advance(slot);
        self.order.submit(index, slot.signal_value);
        buffer.waitUntilCompleted();
        self.slots[slot.index].signal();
        ticket.commit();
        self.in_flight.note_drained(1);
        self.finish(index, slot.signal_value, buffer, device)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn command_buffer(
        &self,
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, EngineError> {
        self.raw
            .commandBuffer()
            .ok_or_else(|| init_error("commandBuffer", "returned nil"))
    }

    pub(crate) fn retire(&mut self, sequence: u64, handle: GpuResourceHandle) {
        self.retire.retire(sequence, handle);
    }

    #[cfg(test)]
    pub(crate) fn retired_handles(&self) -> usize {
        self.retire.pending()
    }

    pub(crate) fn on_completed(
        &mut self,
        index: FrameIndex,
        sequence: u64,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        device: &mut Device,
    ) -> (Option<GpuTimerSample>, Option<FrameIndex>) {
        let sample = self.timer.resolve(buffer);
        let frontier = self.order.note(index, sequence);
        if let Some((_, sequence)) = frontier {
            self.retire.on_completed(sequence, &mut |handle| {
                device.release(handle);
            });
        }
        (sample, frontier.map(|(index, _)| index))
    }

    pub(crate) fn failure(
        &mut self,
        index: FrameIndex,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        device: &mut Device,
    ) -> EngineError {
        let error = buffer.error();
        if let Some(lost) = device.note_failure(error.as_deref(), index.get() as u64) {
            return lost;
        }
        EngineError::Swapchain {
            detail: smol_str::format_smolstr!(
                "MTLCommandBuffer failed: {}",
                error
                    .map(|error| error.localizedDescription().to_string())
                    .unwrap_or_else(|| "unknown MTLCommandBufferError".to_owned())
            ),
        }
    }

    fn finish(
        &mut self,
        index: FrameIndex,
        sequence: u64,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        device: &mut Device,
    ) -> Result<Option<GpuTimerSample>, EngineError> {
        if buffer.status() == MTLCommandBufferStatus::Error {
            return Err(self.failure(index, buffer, device));
        }
        Ok(self.on_completed(index, sequence, buffer, device).0)
    }
}

struct CompletionOrder {
    submitted: VecDeque<(FrameIndex, u64)>,
    completed: Vec<(FrameIndex, u64)>,
}

impl CompletionOrder {
    fn new() -> Self {
        Self {
            submitted: VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT),
            completed: Vec::with_capacity(MAX_FRAMES_IN_FLIGHT),
        }
    }

    fn submit(&mut self, index: FrameIndex, sequence: u64) {
        self.submitted.push_back((index, sequence));
    }

    fn note(&mut self, index: FrameIndex, sequence: u64) -> Option<(FrameIndex, u64)> {
        debug_assert!(
            self.submitted.iter().any(|(_, id)| *id == sequence),
            "completion for sequence {sequence} without a submission"
        );
        self.completed.push((index, sequence));
        let mut frontier = None;
        while let Some(front) = self.submitted.front().copied() {
            if !self.completed.iter().any(|(_, id)| *id == front.1) {
                break;
            }
            self.submitted.pop_front();
            self.completed.retain(|(_, id)| *id != front.1);
            frontier = Some(front);
        }
        frontier
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use objc2_metal::MTLCommandBuffer;

    use super::{CompletionOrder, InFlight, Queue};
    use crate::{render::macos::device::Device, window::FrameIndex};

    fn index(value: u32) -> FrameIndex {
        FrameIndex::from_raw(value)
    }

    #[test]
    fn in_order_completions_advance_immediately() {
        let mut order = CompletionOrder::new();
        for value in 1..=3u64 {
            order.submit(index(value as u32), value);
        }
        assert_eq!(order.note(index(1), 1), Some((index(1), 1)));
        assert_eq!(order.note(index(2), 2), Some((index(2), 2)));
        assert_eq!(order.note(index(3), 3), Some((index(3), 3)));
    }

    #[test]
    fn an_out_of_order_completion_waits_for_the_prefix() {
        let mut order = CompletionOrder::new();
        for value in 1..=3u64 {
            order.submit(index(value as u32), value);
        }
        assert_eq!(order.note(index(3), 3), None);
        assert_eq!(order.note(index(1), 1), Some((index(1), 1)));
        assert_eq!(order.note(index(2), 2), Some((index(3), 3)));
        order.submit(index(4), 4);
        assert_eq!(order.note(index(4), 4), Some((index(4), 4)));
    }

    #[test]
    fn skipped_submissions_do_not_block_the_prefix() {
        let mut order = CompletionOrder::new();
        for (value, sequence) in [(1u32, 1u64), (4, 2), (7, 3)] {
            order.submit(index(value), sequence);
        }
        assert_eq!(order.note(index(7), 3), None);
        assert_eq!(order.note(index(1), 1), Some((index(1), 1)));
        assert_eq!(order.note(index(4), 2), Some((index(7), 3)));
    }

    #[test]
    fn saturated_frame_indices_keep_the_completion_prefix_ordered() {
        let saturated = index(u32::MAX);
        let mut order = CompletionOrder::new();
        for sequence in 1..=3u64 {
            order.submit(saturated, sequence);
        }
        assert_eq!(order.note(saturated, 3), None);
        assert_eq!(order.note(saturated, 2), None);
        assert_eq!(order.note(saturated, 1), Some((saturated, 3)));
        assert!(order.submitted.is_empty(), "前缀推进后提交 FIFO 清空");
    }

    #[test]
    fn the_in_flight_account_is_already_positive_inside_the_completed_handler() {
        let device = Device::new().expect("metal device");
        let in_flight = Arc::new(InFlight::new());
        let mut queue = Queue::new(&device, Arc::clone(&in_flight)).expect("queue");
        let slot = queue.try_acquire().expect("upload slot");
        let buffer = queue.begin().expect("command buffer");
        let observed = Arc::new(AtomicUsize::new(usize::MAX));
        let counter = Arc::clone(&in_flight);
        let probe = Arc::clone(&observed);
        queue
            .submit(index(1), slot, &buffer, move |_buffer| {
                probe.store(counter.get(), Ordering::Release);
            })
            .expect("submit");
        assert_eq!(in_flight.get(), 1, "记账必须在提交路径上、完成回调之前完成");
        buffer.waitUntilCompleted();
        let deadline = Instant::now() + Duration::from_secs(5);
        while observed.load(Ordering::Acquire) == usize::MAX && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            observed.load(Ordering::Acquire) >= 1,
            "完成回调看到 in_flight = 0：记账晚于 commit（R09-17 回归）"
        );
        let completions = observed.load(Ordering::Acquire);
        assert!(completions >= 1);
        in_flight.note_drained(1);
        assert_eq!(in_flight.get(), 0, "drain 之后归零");
    }

    #[test]
    fn a_submission_that_never_commits_rolls_its_account_back() {
        let in_flight = Arc::new(InFlight::new());
        {
            let _ticket = in_flight.note_submitted();
            assert_eq!(in_flight.get(), 1, "记账在提交开始时转正");
        }
        assert_eq!(
            in_flight.get(),
            0,
            "令牌被 drop（没有 commit）必须回滚这次记账"
        );
    }

    #[test]
    #[should_panic(expected = "in-flight accounting underflow")]
    fn draining_more_completions_than_submitted_is_a_hard_error() {
        let in_flight = InFlight::new();
        in_flight.note_drained(1);
    }
}
