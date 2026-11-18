//! 命令队列 / 三槽上传背压 / 异步提交 / 完成通知 / retire。
//!
//! **没有在飞数组、没有 `status()` 轮询、没有 `waitUntilCompleted` 背压**：
//! - 提交：登记 `addCompletedHandler`（signal 三槽 semaphore + 把命令缓冲交给调用方）后 `commit`，
//!   立即返回（I2）；
//! - 槽位复用：`dispatch_semaphore`(3) 用 `DISPATCH_TIME_NOW` **非阻塞**获取（I1），拿不到就是
//!   跳帧；`signal` 只在完成回调里；
//! - 延迟销毁：旧 persist / 旧上传缓冲按 **`u64` 提交序号**（`FrameSlot::signal_value`）入
//!   `RetireQueue`，**连续完成前缀**推进后才释放（I4；Metal 只保证 command buffer 的开始顺序，
//!   完成回调可能乱序）。**不用 `FrameIndex`**（R06-1）：`u32` 帧号饱和后同一帧号代表所有后续帧，
//!   按它记账会提前释放仍在飞的资源。
//!
//! 三槽 semaphore 保护的是**三槽上传缓冲与 GPU 队列深度**（在飞 command buffer 超过 3 个就会
//! 覆盖别人的实例字节），不是 drawable 池 —— drawable 由 `CAMetalDisplayLink` 提供。
//!
//! 离屏/快照路径仍是同步语义（§4.5 下半段）：`submit_blocking` 在 GPU 线程上等本帧完成，
//! 那里没有 UI 线程可阻塞。

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

/// 在飞帧计数（UI 线程读、GPU 线程写；Metal 的完成回调不碰它）。
///
/// 记账语义（R09-17）：`note_submitted` 发生在 `addCompletedHandler` **之前**（Metal 只在
/// `commit` 之后才可能回调完成），`note_drained` 只由 GPU 线程在 drain 完成队列时按入队数扣减
/// —— 计数与完成队列因此永远同源，不存在"完成先到、账未记"的窗口期。
///
/// 用原子而不是 `Shared` 的互斥量：`sync_pacing`（主线程，每轮）与提交路径（GPU 线程）都要读
/// 它，而它不需要与其它状态一起原子地读。
pub(crate) struct InFlight {
    count: AtomicUsize,
}

impl InFlight {
    pub(crate) fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    /// 提交前记账 +1，返回必须在 `commit` 之后 [`SubmissionTicket::commit`] 的令牌。
    ///
    /// 令牌是 RAII 的：这条路径上任何提前返回（包括将来新增的失败分支）都会在 `Drop` 里回滚
    /// 这 +1，因此"先记账"不会留下漏账。
    ///
    /// 上界：未 drain 的完成通知最多 `MAX_FRAMES_IN_FLIGHT` 个（每个槽的 permit 在它的完成回调
    /// 里归还，GPU 线程每轮先 drain 再提交），加上仍在飞的提交最多 `MAX_FRAMES_IN_FLIGHT` 个
    /// ⇒ 计数上界是 `MAX_FRAMES_IN_FLIGHT * 2`。超过它说明有人绕过 `Queue` 记账。
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

    /// GPU 线程 drain 完成通知：减去同样多。
    ///
    /// 计数错误（扣得比记的多）是记账 bug，必须立刻可见 —— 旧实现的 `saturating_sub` 会把它
    /// 静默钳到 0，让"关闭握手提前退出 / pacing 抖动"这类问题无从复现（R09-17）。
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

    /// 已提交、尚未 drain 的帧数。
    pub(crate) fn get(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// 测试：直接把计数设成 `count`（模拟"有 N 帧在飞"的状态）。
    #[cfg(test)]
    pub(crate) fn preset(&self, count: usize) {
        self.count.store(count, Ordering::Release);
    }
}

/// "本次提交已记账、等 `commit` 转正"的令牌（见 [`InFlight::note_submitted`]）。
struct SubmissionTicket {
    ledger: Arc<InFlight>,
    committed: bool,
}

impl SubmissionTicket {
    /// `commit` 已返回：记账正式生效，`Drop` 不再回滚。
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for SubmissionTicket {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // 回滚：这一次提交没有走到 `commit`，它的完成通知永远不会来。
        self.ledger.count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 一个命令队列 + 三槽上传背压 + 延迟销毁队列。
pub(crate) struct Queue {
    raw: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// 与 UI 线程共享的在飞帧计数（[`InFlight`]）。
    in_flight: Arc<InFlight>,
    ring: InFlightRing,
    /// **每个上传槽一把** semaphore（各 1 个 permit），Apple「Synchronizing CPU and GPU work」
    /// 三缓冲模式的标准形态。
    ///
    /// P1-2.12 修正：旧实现是**一把** 3-permit 的 semaphore，permit 在完成回调里归还，而槽位按
    /// **提交次数**轮转。Metal 的完成回调**可以乱序**到达 —— 第 3 帧先完成就归还一个 permit，
    /// 第 4 帧立刻拿到它并轮转回**第 1 槽**，而第 1 帧可能还在 GPU 上读那块上传缓冲，于是在
    /// CPU 上重写了 GPU 正在读的字节（内容损坏，Metal validation 检测不到）。
    ///
    /// 按槽绑定 permit 之后，「这一槽的上一帧完成」是它唯一的复用前提，与其它槽的完成顺序
    /// 完全无关 —— 不需要 `MTLSharedEvent`，也不需要假设同队列完成有序。
    slots: [DispatchRetained<DispatchSemaphore>; MAX_FRAMES_IN_FLIGHT],
    /// retire 的完成顺序（Metal 的完成回调可以乱序到达，见 [`CompletionOrder`]）。
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

    /// 非阻塞取一个上传槽；`None` = 三帧都在飞，本帧跳帧（帧请求留在最新帧槽里）。
    ///
    /// 返回的 `FrameSlot` 与成功取得的 permit **一一对应**：调用方必须把它原样交给
    /// [`Queue::begin`] + [`Queue::submit`]（或用 [`Queue::release`] 归还），否则该槽的
    /// permit 会永久泄漏、三帧后彻底停止出帧。
    pub(crate) fn try_acquire(&mut self) -> Option<FrameSlot> {
        // 先按轮转算出「下一个要用的槽」，只在**该槽自己的**上一帧完成后才放行。
        let candidate = self.ring.peek();
        if self.slots[candidate.index].wait(DispatchTime::NOW) != 0 {
            return None;
        }
        Some(self.ring.acquire())
    }

    /// 归还一个已经取到、但本帧没有提交的槽位（防御路径，不与 `submit` 同时用）。
    pub(crate) fn release(&self, slot: FrameSlot) {
        self.slots[slot.index].signal();
    }

    /// 建一块命令缓冲：**不等待任何 GPU 工作**。
    ///
    /// 槽位不在这里分配 —— 它由 [`Queue::try_acquire`] 给出，permit 与上传槽必须同一来源
    /// （P1-2.12）；调用方把那个槽交给 `upload.write` 与 [`Queue::submit`]。
    pub(crate) fn begin(
        &mut self,
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, EngineError> {
        let buffer = self
            .raw
            .commandBuffer()
            .ok_or_else(|| init_error("commandBuffer", "returned nil"))?;
        Ok(buffer)
    }

    /// 登记完成通知并 `commit`：立即返回（I2）。
    ///
    /// `completed` 在 Metal 的完成回调里执行（任意线程），因此要求 `Send + 'static`；
    /// 回调先归还**本槽**的 permit，再把命令缓冲交给调用方（GPU 线程用它在 drain 时读 GPU 时间）。
    ///
    /// **记账顺序（R09-17）**：`in_flight += 1` 在 `addCompletedHandler` **之前**。Metal 保证
    /// 命令缓冲的完成回调只可能在 `commit` 之后被调用，所以"先记账、后提交"不存在"完成先到、
    /// 账未记"的窗口期；主线程的 `sync_pacing` 不会在刚提交完的那一瞬读到 0 而误暂停 display
    /// link。任何提前返回都会由 [`SubmissionTicket`] 的 `Drop` 回滚这次记账。
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
                // 只归还**这一槽**的 permit：与其它槽的完成顺序无关（P1-2.12）。
                permit.signal();
                // SAFETY: 参数是 `NonNull`（Metal 保证完成回调收到的命令缓冲非空），且回调期间
                // 该对象存活；这里 retain 一份交给 GPU 线程在 drain 时读 `GPUStartTime`/`GPUEndTime`。
                let buffer =
                    unsafe { Retained::retain(buffer.as_ptr()) }.expect("Metal 的完成回调参数非空");
                completed(buffer);
            },
        );
        // SAFETY: `addCompletedHandler:` 只借用 block 指针，并在返回前拷贝一份；`block` 在本帧存活。
        unsafe { buffer.addCompletedHandler(RcBlock::as_ptr(&block)) };
        buffer.commit();
        self.ring.advance(slot);
        self.order.submit(index, slot.signal_value);
        debug_assert_eq!(
            self.ring.last_signaled(),
            slot.signal_value,
            "ring must advance exactly one step per submitted frame"
        );
        // 到这里命令缓冲已经在队列里：完成通知一定会来，记账转正（`Drop` 不再回滚）。
        ticket.commit();
        Ok(())
    }

    /// 离屏/快照：同步提交（`commit` 后等本帧完成），供 readback 保证内容可读。
    ///
    /// 这是**同步语义**，不是背压：窗口路径永远不经过这里（§4.5 下半段）。
    ///
    /// 因为 `waitUntilCompleted` 已经保证本帧结束，这里**同步归还**本槽的 permit
    /// （P1-2.12）—— 走异步路径的 [`Queue::submit`] 才在完成回调里归还。不归还的话
    /// 每次离屏提交都会永久吃掉一个 permit，三次之后彻底取不到上传槽。
    pub(crate) fn submit_blocking(
        &mut self,
        index: FrameIndex,
        slot: FrameSlot,
        buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        device: &mut Device,
    ) -> Result<Option<GpuTimerSample>, EngineError> {
        // 同步路径没有完成回调：记一次、等完之后立刻扣掉，计数在调用返回时回到原值。
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

    /// 帧外的一次性命令缓冲（readback 的 blit）：**不**动 ring 的槽位。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn command_buffer(
        &self,
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, EngineError> {
        self.raw
            .commandBuffer()
            .ok_or_else(|| init_error("commandBuffer", "returned nil"))
    }

    /// 延迟销毁：本提交用过的资源至少要活到 `sequence`（提交序号）的 GPU 工作完成。
    ///
    /// 键是 `InFlightRing` 的 `u64` 提交序号，**不是** `FrameIndex`（R06-1）：`u32` 帧号饱和后
    /// 同一帧号代表所有后续帧，按它记账会提前释放仍在飞的资源。
    pub(crate) fn retire(&mut self, sequence: u64, handle: GpuResourceHandle) {
        self.retire.retire(sequence, handle);
    }

    /// 尚未释放的 retire 句柄数（GPU 测试断言「在飞帧的 staging 不得提前释放」）。
    #[cfg(test)]
    pub(crate) fn retired_handles(&self) -> usize {
        self.retire.pending()
    }

    /// 完成通知到达（GPU 线程 drain）：读 GPU 时间 + 推进**连续前缀**的 retire。
    ///
    /// `index` 只用于反馈 / 诊断；retire 的键是 `sequence`（提交序号，见 [`Queue::retire`]）。
    /// 返回 GPU 采样与新的**连续完成前沿**（只有连续前缀推进时才发布反馈，见
    /// `renderer::FeedbackQueue`；没有推进时为 `None`）。
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

    /// 已提交命令缓冲的失败分类：设备丢失（粘性）/ 其余 `Swapchain`。
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

    /// 同步提交的收尾：失败分类 + GPU 计时 + retire。
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

/// retire 的完成顺序：已提交的 `(帧序号, 提交序号)` FIFO + 已完成集合，只有**连续前缀**完成才推进。
///
/// Apple 只保证同一队列 command buffer 的**开始**顺序（提交顺序），不保证完成回调的顺序。
/// 直接按回调的提交序号推进 [`RetireQueue`] 会在较大序号先到时释放仍被较早帧使用的旧 persist
/// （旧 persist 还在较早帧的命令缓冲里被读）。Windows / Linux 的单调 fence / timeline 天然
/// 满足这个顺序，因此本结构只在 macOS 需要。
///
/// **身份用提交序号而不是帧序号**（R06-1）：帧序号会饱和（同一帧号重复出现），提交序号由
/// `InFlightRing` 每提交一次递增，永远唯一。帧序号只作为反馈标签随前沿一起返回。
struct CompletionOrder {
    /// 已提交、尚未推进 retire 的 `(帧序号, 提交序号)`（提交顺序）。
    submitted: VecDeque<(FrameIndex, u64)>,
    /// 已完成、但前面还有未完成提交的条目（在飞帧最多 `MAX_FRAMES_IN_FLIGHT` 个）。
    completed: Vec<(FrameIndex, u64)>,
}

impl CompletionOrder {
    fn new() -> Self {
        Self {
            submitted: VecDeque::with_capacity(MAX_FRAMES_IN_FLIGHT),
            completed: Vec::with_capacity(MAX_FRAMES_IN_FLIGHT),
        }
    }

    /// 记录一次提交（`commit` 之后调用）。
    fn submit(&mut self, index: FrameIndex, sequence: u64) {
        self.submitted.push_back((index, sequence));
    }

    /// 记录一次完成，返回新到达的**连续完成前沿**（`(帧序号, 提交序号)`；没有推进时为 `None`）。
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
        // 3 先完成：前沿不动（不能释放 3 引用的旧资源）。
        assert_eq!(order.note(index(3), 3), None);
        // 1 完成：前沿到 1（2 还没完成，不能跨过它）。
        assert_eq!(order.note(index(1), 1), Some((index(1), 1)));
        // 2 完成：1..=3 全部完成，前沿一次推到 3。
        assert_eq!(order.note(index(2), 2), Some((index(3), 3)));
        // 之后的提交从新的前缀继续。
        order.submit(index(4), 4);
        assert_eq!(order.note(index(4), 4), Some((index(4), 4)));
    }

    #[test]
    fn skipped_submissions_do_not_block_the_prefix() {
        // 被合并掉的序号不产生提交，因此也不在 FIFO 里；前缀只按真正提交的序号推进。
        let mut order = CompletionOrder::new();
        for (value, sequence) in [(1u32, 1u64), (4, 2), (7, 3)] {
            order.submit(index(value), sequence);
        }
        assert_eq!(order.note(index(7), 3), None);
        assert_eq!(order.note(index(1), 1), Some((index(1), 1)));
        assert_eq!(order.note(index(4), 2), Some((index(7), 3)));
    }

    /// R06-1：`FrameIndex` 饱和（同一帧号反复出现）不会打乱完成顺序 —— 身份用 `u64` 提交序号，
    /// 帧号只作为反馈标签随前沿返回。
    #[test]
    fn saturated_frame_indices_keep_the_completion_prefix_ordered() {
        let saturated = index(u32::MAX);
        let mut order = CompletionOrder::new();
        for sequence in 1..=3u64 {
            order.submit(saturated, sequence);
        }
        // 3、2 先完成：前缀（1）未完成，不能推进。
        assert_eq!(order.note(saturated, 3), None);
        assert_eq!(order.note(saturated, 2), None);
        // 1 完成：前缀一次推进到 3。
        assert_eq!(order.note(saturated, 1), Some((saturated, 3)));
        assert!(order.submitted.is_empty(), "前缀推进后提交 FIFO 清空");
    }

    /// R09-17：完成回调被调用时，`in_flight` 必须**已经**记上了这一次提交。
    ///
    /// 旧实现在 `commit` 返回之后、回到 GPU 线程才 `+1`，完成回调可以在那个窗口期里先跑：
    /// 回调看到 0 ⇒ `sync_pacing` 误判空闲、关闭握手可能提前退出。这里在回调里直接读计数，
    /// 是这条顺序的最直接回归。
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
        // 提交之后计数立刻为 1（不等任何完成回调）。
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
        // 完成通知被 GPU 线程 drain：按 drain 数扣减（与 `Shared::take_completions` 同一路径）。
        in_flight.note_drained(1);
        assert_eq!(in_flight.get(), 0, "drain 之后归零");
    }

    /// 没走到 `commit` 的提交必须回滚它记的那一笔（`SubmissionTicket` 的 `Drop`）。
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

    /// 扣得比记的多是记账 bug：必须是硬错误，不能被静默钳到 0（旧实现用 `saturating_sub`）。
    #[test]
    #[should_panic(expected = "in-flight accounting underflow")]
    fn draining_more_completions_than_submitted_is_a_hard_error() {
        let in_flight = InFlight::new();
        in_flight.note_drained(1);
    }
}
