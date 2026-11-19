//! 每窗口提交流：共享 `ID3D12CommandQueue` 上的 allocator / list / fence 记账 / retire 排空
//!
//! 进程级只有**一条** direct queue（`device::SharedGpu`）。每个窗口保留自己的 in-flight 记账：
//! 三个 allocator + 三个命令列表 + 自己的 fence（提交后 `Signal`）。D3D12 的 command queue 是
//! free-threaded 的：不同线程 `ExecuteCommandLists` 到同一队列是合法的，且队列保证先提交的
//! 工作先完成。命令列表按提交顺序整段执行，因此图集上传的屏障序列不会和别的窗口交错。
//!
//! **提交是异步的**（I2）：`submit` 关闭列表、`ExecuteCommandLists`、`Signal` 之后立刻返回，
//! 完成状态只通过 fence 观察。
//!
//! **等待的结果分两类**（R08-3）：
//! - [`WaitOutcome::Signaled`]：fence 到达；
//! - [`WaitOutcome::TimedOut`]：`FENCE_TIMEOUT_MS` 内没到，但 `GetDeviceRemovedReason()` 是
//!   `S_OK` ⇒ **设备健康**，只是一次时序现象（锁屏 / 熄屏 / RDP / 合成器停摆 / 提交被挤）。
//!   这不是错误：调用方退避、跳过这一帧或下一次重试，绝不升级成粘性 `DeviceLost`。
//!
//! 只有 `DXGI_ERROR_DEVICE_REMOVED/_HUNG/_RESET/DRIVER_INTERNAL_ERROR`（[`super::hresult`]）
//! 才会升级成 `EngineError::DeviceLost`（进程级粘性状态，由 `SharedGpu` 持有），并进入 R08-4 的
//! 恢复路径。
//!
//! 另外，每个提交流向 [`SharedGpu`] 登记一条**水位**（[`StreamWatermark`]：fence + 最后一次
//! 提交的 signal 值）。图集页换代（R08-6）只需要等这些水位，而不是等整条共享队列 idle：
//! 页换代发生在图集**写锁**内，而所有窗口的「录制 + 提交」都在图集**读锁**内，因此写锁期间
//! 不可能出现新的提交 —— 快照时的每条水位就覆盖了「引用旧纹理的全部在飞工作」。

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

/// fence 等待的硬上限（P1-2.13）。INFINITE 会在设备被移除时永久挂住 UI 线程；
/// 2 s 是 GPU watchdog 的常规量级，健康的帧远达不到。
pub(crate) const FENCE_TIMEOUT_MS: u32 = 2_000;

/// 一次有界等待的结果（R08-3：超时**不是**错误）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    /// fence 已到达（或等待值为 0，从未使用过的槽）。
    Signaled,
    /// 设备健康，但 fence 在 [`FENCE_TIMEOUT_MS`] 内没到。
    TimedOut,
}

impl WaitOutcome {
    /// 是否等到了。
    pub(crate) fn is_signaled(self) -> bool {
        matches!(self, Self::Signaled)
    }
}

/// 一个提交流的**水位**：它的 fence + 最后一次提交的 signal 值 + 每一代图集描述符组的弃用水位。
///
/// 共享侧（[`SharedGpu`]）只持有 `Weak`：窗口销毁后条目自然失效。图集页换代用它界定
/// 「哪些在飞工作引用了旧纹理」（C3 双代描述符）。
pub(crate) struct StreamWatermark {
    fence: ID3D12Fence,
    value: AtomicU64,
    /// 该流最后一次**使用某一代**图集描述符组时的提交序号（0 = 从未使用）。
    ///
    /// 换代（active → target）时由 `SharedGpu` 把各流的当前 `value` 写进**被弃用**那一代；
    /// 只有 `fence.GetCompletedValue() >= 该值` 时，该代才允许被重新写入 + 重新启用
    /// （此时该代的全部在飞引用都已执行完）。
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

    /// 登记「本流最后一次使用 `generation` 代」的提交序号（C3；由页换代在**图集写锁内**调用，
    /// 此时本流不可能有新提交）。
    pub(crate) fn retire_generation(&self, generation: u8, value: u64) {
        debug_assert!(
            (generation as usize) < self.retired.len(),
            "atlas generation out of range"
        );
        self.retired[generation as usize].store(value, Ordering::Release);
    }

    /// 该代是否已无在飞引用（非阻塞：只读 fence 完成值）。从未被弃用的代恒为真。
    pub(crate) fn generation_idle(&self, generation: u8) -> bool {
        let retired = match self.retired.get(generation as usize) {
            Some(retired) => retired.load(Ordering::Acquire),
            None => return true,
        };
        if retired == 0 {
            return true;
        }
        // SAFETY: fence 由本流持有，调用期间有效；只读查询完成值。
        (unsafe { self.fence.GetCompletedValue() }) >= retired
    }

    /// 提交成功后推进水位（`Release`：页换代在写锁内 `Acquire` 读取）。
    fn publish(&self, value: u64) {
        self.value.store(value, Ordering::Release);
    }

    /// 当前水位（0 = 该流从未提交）。
    pub(crate) fn value(&self) -> u64 {
        self.value.load(Ordering::Acquire)
    }

}

/// 一个窗口在共享队列上的提交流：allocator/list + 自己的 fence + 提交记账 + retire。
pub(crate) struct CommandStream {
    shared: Arc<SharedGpu>,
    allocators: Vec<ID3D12CommandAllocator>,
    lists: Vec<ID3D12GraphicsCommandList>,
    fence: ID3D12Fence,
    event: HANDLE,
    ring: InFlightRing,
    /// 已提交但**尚未确认完成**的提交序号（FIFO：`Signal` 的 fence 值）。
    submitted: VecDeque<u64>,
    retire: RetireQueue,
    /// retire 队列里出现过的最大提交序号（teardown 的兜底释放用）。
    last_retire_sequence: Option<u64>,
    /// 向共享设备登记的提交水位（图集页换代的最小等待集，R08-6）。
    watermark: Arc<StreamWatermark>,
}

impl Drop for CommandStream {
    fn drop(&mut self) {
        self.shared.unregister_watermark(&self.watermark);
        // SAFETY: `event` 由本结构独占；`Drop` 只执行一次，关闭后不再使用。
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
            // SAFETY: 设备句柄由进程级 `SharedGpu` 持有，调用期间有效；错误按 HRESULT 返回。
            let allocator: ID3D12CommandAllocator = unsafe {
                shared
                    .raw()
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            }
            .map_err(|error| init_error_from("CreateCommandAllocator", error))?;
            // SAFETY: 同上；allocator 在列表 close 之后独占。
            let list: ID3D12GraphicsCommandList = unsafe {
                shared
                    .raw()
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            }
            .map_err(|error| init_error_from("CreateCommandList", error))?;
            // SAFETY: 列表由本结构独占；创建后立即关闭，等 `reset_slot` 才重新打开。
            unsafe { list.Close() }.map_err(|error| init_error_from("Close", error))?;
            allocators.push(allocator);
            lists.push(list);
        }

        // SAFETY: 设备句柄由进程级 `SharedGpu` 持有，调用期间有效；错误按 HRESULT 返回。
        let fence: ID3D12Fence = unsafe { shared.raw().CreateFence(0, D3D12_FENCE_FLAG_NONE) }
            .map_err(|error| init_error_from("CreateFence", error))?;
        // `CreateEventW` 需要 `Win32_Security` feature（DEVIATIONS F-79）。
        // SAFETY: 无名称的 auto-reset 事件；句柄由本结构独占并在 `Drop` 关闭。
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

    /// 本帧的槽位与 fence 值：槽位按**提交次数**轮转（与三槽 allocator/list 同源），
    /// 提交时才推进 fence 记账。
    pub(crate) fn acquire_slot(&mut self) -> FrameSlot {
        self.ring.acquire()
    }

    /// 等到 `value` 到达（`0` = 该槽从未使用，直接返回）。
    ///
    /// 超时返回 [`WaitOutcome::TimedOut`]（**设备健康**；调用方退避或重试），只有真正的设备
    /// 移除才返回 `Err`。
    ///
    /// 只允许在 GPU 线程（复用 allocator 的前置条件）或离屏同步路径（读回 / 退出）调用。
    pub(crate) fn wait_slot_fence(&self, value: u64) -> Result<WaitOutcome, EngineError> {
        self.wait_for(value)
    }

    /// 复位该槽的 allocator + list（调用方必须先用 `WaitOutcome::Signaled` 等到槽空闲）。
    pub(crate) fn reset_slot(&mut self, slot: FrameSlot) -> Result<(), EngineError> {
        let allocator = &self.allocators[slot.index];
        // SAFETY: 该槽上一次提交已由 `wait_slot_fence` 等到完成，allocator 不再被 GPU 使用。
        unsafe { allocator.Reset() }
            .map_err(|error| init_error_from("CommandAllocator::Reset", error))?;
        let list = &self.lists[slot.index];
        // SAFETY: 句柄由本结构持有；列表上一次已关闭。
        unsafe { list.Reset(allocator, None) }
            .map_err(|error| init_error_from("CommandList::Reset", error))?;
        Ok(())
    }

    /// 正在录制的命令列表（`reset_slot` 之后有效）。
    pub(crate) fn list(&self, slot: FrameSlot) -> &ID3D12GraphicsCommandList {
        &self.lists[slot.index]
    }

    /// 关闭 → 执行 → `Signal`，**立即返回**（I2：完成通知只走 fence）。
    pub(crate) fn submit(&mut self, slot: FrameSlot) -> Result<(), EngineError> {
        let list = &self.lists[slot.index];
        // SAFETY: 列表由本结构独占，提交前已录制完成。
        unsafe { list.Close() }.map_err(|error| init_error_from("CommandList::Close", error))?;
        let command_list: ID3D12CommandList = list
            .cast()
            .map_err(|error| init_error_from("cast ID3D12CommandList", error))?;
        // SAFETY: 命令列表已关闭；队列是进程级共享队列（free-threaded，允许并发提交）。
        unsafe {
            self.shared
                .queue()
                .ExecuteCommandLists(&[Some(command_list)])
        };
        // SAFETY: fence 由本结构持有；Signal 只入队。
        unsafe { self.shared.queue().Signal(&self.fence, slot.signal_value) }
            .map_err(|error| init_error_from("CommandQueue::Signal", error))?;
        self.ring.advance(slot);
        self.submitted.push_back(slot.signal_value);
        // 水位与本结构共享：页换代只需要等它，不必等整条队列（R08-6）。
        self.watermark.publish(slot.signal_value);
        Ok(())
    }

    /// 请求在 `value` 到达时置位 `event`（`ID3D12Fence::SetEventOnCompletion`）。
    ///
    /// 这是**完成通知**的原语（§6.5 / §8.1），不是等待：GPU 线程随后用
    /// `WaitForMultipleObjects([work, completion])` 睡到有活或本帧完成，不轮询 `GetCompletedValue`。
    /// 若 fence 已经越过 `value`，Windows 会立即置位该事件。
    pub(crate) fn set_completion_event(
        &self,
        value: u64,
        event: HANDLE,
    ) -> Result<(), EngineError> {
        // SAFETY: fence 句柄由本结构持有到 `Drop`；`event` 由调用方保证在 `value` 到达前存活
        // （Windows 层里它是 `Shared` 的 auto-reset 事件，晚于 GPU 线程释放）。
        unsafe { self.fence.SetEventOnCompletion(value, event) }
            .map_err(|error| init_error_from("SetEventOnCompletion(completion)", error))
    }

    /// fence 的已完成值（非阻塞；`release_completed` 与 GPU 循环的完成判据）。
    pub(crate) fn completed_value(&self) -> u64 {
        // SAFETY: fence 句柄由本结构持有；查询无副作用。
        unsafe { self.fence.GetCompletedValue() }
    }

    /// 等到本窗口最后一次提交完成（离屏 resize / 读回 / 退出 / `ResizeBuffers` 前）。
    ///
    /// 只等**本提交流**的 fence：别的窗口在共享队列上还有在飞工作时，本窗口的资源仍可复用
    /// （allocator / list / persist 都是每窗口对象）。共享图集的页换代走
    /// `SharedGpu::rebuild_atlas_pages` 的**每窗口水位**等待，不是这里。
    pub(crate) fn wait_idle(&mut self) -> Result<WaitOutcome, EngineError> {
        self.wait_for(self.ring.last_signaled())
    }

    /// 复位**所有** in-flight 槽的 allocator + list 并立即关闭列表（只用于退出 / 重建路径）。
    ///
    /// 关闭的 command list 仍记录着它引用过的资源；不 Reset 的话 swapchain 的 back buffer 会一直
    /// 有引用（三个 buffer 全部泄漏，swapchain 也带着引用被销毁，DEVIATIONS F-86）。调用方必须
    /// 先**等到** `wait_idle` 返回 `Signaled`（allocator 在 GPU 未完成时不可 Reset）。
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

    /// 帧外的一次性提交（读回 / 图集整页上传）：等 idle → 用槽 0 录制 → 提交 → 等完成。
    ///
    /// 调用前后都必须 idle；提交成功后把槽 0 的 fence 记账推进到本次 signal 值。
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
        // SAFETY: 槽 0 空闲（上一提交已等到），allocator 不再被 GPU 使用。
        unsafe { allocator.Reset() }
            .map_err(|error| init_error_from("CommandAllocator::Reset", error))?;
        let list = &self.lists[0];
        // SAFETY: 列表已关闭；重开后由录制闭包独占。
        unsafe { list.Reset(allocator, None) }
            .map_err(|error| init_error_from("CommandList::Reset", error))?;
        record(list)?;
        // SAFETY: 录制完成，列表由本函数独占。
        unsafe { list.Close() }.map_err(|error| init_error_from("CommandList::Close", error))?;
        let command_list: ID3D12CommandList = list
            .cast()
            .map_err(|error| init_error_from("cast ID3D12CommandList", error))?;
        // SAFETY: 队列 / fence 句柄由进程级 `SharedGpu` 持有；命令列表已关闭。
        unsafe {
            self.shared
                .queue()
                .ExecuteCommandLists(&[Some(command_list)])
        };
        // 帧外提交也占一个单调递增的 fence 值，并把**实际使用的槽 0** 的记账推进到该值：
        // 调用前的 `wait_idle` 已等到最后提交值（≥ 槽 0 的上一帧），因此 `wait_value` 就是那条
        // 已经满足的前置条件。
        let slot = FrameSlot {
            index: 0,
            wait_value: self.ring.last_signaled(),
            signal_value: self.ring.last_signaled() + 1,
        };
        // SAFETY: fence 由本结构持有；Signal 只入队。
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

    /// 延迟销毁：本提交用过的资源至少要活到 `sequence`（fence 的 signal 值）完成。
    ///
    /// 键是 fence 的 `u64` 提交序号，**不是** `FrameIndex`（R06-1）：`u32` 帧号饱和后同一个帧号
    /// 代表所有后续帧，按它记账会在 `GetCompletedValue()` 越过饱和点时提前释放仍在飞的资源。
    pub(crate) fn retire(&mut self, sequence: u64, handle: GpuResourceHandle) {
        self.last_retire_sequence = Some(
            self.last_retire_sequence
                .map_or(sequence, |last| last.max(sequence)),
        );
        self.retire.retire(sequence, handle);
    }

    /// teardown：GPU 已空闲（调用方先 `wait_idle`），把 retire 队列里**所有**残留句柄交回
    /// `SharedGpu` —— 包括录制失败、从未提交的帧（正常路径不会观察到它们的完成）。
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

    /// **非阻塞**推进 retire：fence 已到达的提交（含被跳过的序号，§4.3）及更早的句柄交回
    /// `SharedGpu` 真正释放。返回释放数。
    ///
    /// 只读 `GetCompletedValue`，不等 GPU（I2 / I4）。
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

    /// 尚未释放的句柄数（诊断 / 测试）。
    #[cfg(test)]
    pub(crate) fn pending_retire(&self) -> usize {
        self.retire.pending()
    }

    pub(crate) fn timestamp_frequency(&self) -> Result<u64, EngineError> {
        // SAFETY: 队列句柄由进程级 `SharedGpu` 持有，调用期间有效；查询无副作用。
        unsafe { self.shared.queue().GetTimestampFrequency() }
            .map_err(|error| init_error_from("GetTimestampFrequency", error))
    }

    /// 有界等待（R08-3）：超时只在**设备健康**时返回 [`WaitOutcome::TimedOut`]。
    fn wait_for(&self, value: u64) -> Result<WaitOutcome, EngineError> {
        if value == 0 || self.completed_value() >= value {
            return Ok(WaitOutcome::Signaled);
        }
        // SAFETY: fence / event 句柄由本结构持有，调用期间有效。
        unsafe { self.fence.SetEventOnCompletion(value, self.event) }
            .map_err(|error| init_error_from("SetEventOnCompletion", error))?;
        // **有界等待**（P1-2.13）：旧实现传 `INFINITE`。设备被移除 / 驱动挂死时
        // `SetEventOnCompletion` 永远不会触发，UI 线程就永久阻塞 —— 窗口无响应且没有任何
        // 错误上报。
        // SAFETY: 同上。
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

    /// 超时分类（R08-3）：**只有**真正的设备移除才升级成粘性 `DeviceLost`。
    ///
    /// `GetDeviceRemovedReason() == S_OK` 时设备健康（常见于锁屏 / 熄屏 / RDP 切换 / 合成器
    /// 停摆），这是一次可恢复的时序现象：计数 + 让调用方退避重试。
    fn classify_timeout(&self) -> Result<WaitOutcome, EngineError> {
        // SAFETY: 设备句柄由 `SharedGpu` 持有，调用期间有效；查询无副作用。
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
                // 其它原因（例如 `INVALID_CALL` 之类的驱动怪癖）：设备仍然可用。
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
            // 提交本身不等 GPU（I2）：完成状态由调用方按 fence 观察。
            assert_eq!(
                stream.wait_slot_fence(slot.signal_value).expect("signaled"),
                WaitOutcome::Signaled
            );
        }
        assert_eq!(stream.wait_idle().expect("idle"), WaitOutcome::Signaled);
        assert_eq!(stream.pending_retire(), 0);
    }

    /// 水位随提交推进（图集页换代的最小等待集，R08-6）：`SharedGpu` 看到的正是最后 signal 值。
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

    /// R06-1：retire 的键是 fence 的 `u64` 提交序号，不是 `u32` 帧序号。
    ///
    /// 两件事一起守：(1) 常规路径按 fence 值释放；(2) 超出 `u32` 范围的提交点（帧序号饱和之后
    /// 的常态）不会被「更早的帧已完成」提前放掉 —— 只有 teardown 的 `drain_retire` 兜底释放。
    ///
    /// 句柄只用于记账（`u32::MAX` 槽位不存在，`release` 是无害的 no-op），断言的是队列行为。
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

        // 本提交点：fence 已到 ⇒ 按提交序号释放。
        stream.retire(slot.signal_value, handle);
        assert_eq!(
            stream.release_completed(),
            1,
            "fence 已到的提交点按 u64 序号释放"
        );
        assert_eq!(stream.pending_retire(), 0);

        // 远超 `u32` 范围的提交点：fence 没到 ⇒ 不得释放（键若还是 `u32` 帧号，饱和后两者同号）。
        let future = u64::from(u32::MAX) + 7;
        stream.retire(future, handle);
        assert_eq!(stream.release_completed(), 0, "fence 没到那个提交点");
        assert_eq!(
            stream.pending_retire(),
            1,
            "键是 u64 提交序号，不会被已完成的更早提交放掉"
        );
        // teardown：调用方已 idle，残留句柄兜底释放。
        stream.drain_retire();
        assert_eq!(stream.pending_retire(), 0);
    }
}
