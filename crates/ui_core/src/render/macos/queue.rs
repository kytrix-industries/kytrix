//! 命令队列 / 三槽上传背压 / 异步提交 / 完成通知 / retire（`design/async-render-pipeline.md` §5.2）。
//!
//! **没有在飞数组、没有 `status()` 轮询、没有 `waitUntilCompleted` 背压**：
//! - 提交：登记 `addCompletedHandler`（signal 三槽 semaphore + 把命令缓冲交给调用方）后 `commit`，
//!   立即返回（I2）；
//! - 槽位复用：`dispatch_semaphore`(3) 用 `DISPATCH_TIME_NOW` **非阻塞**获取（I1），拿不到就是
//!   跳帧；`signal` 只在完成回调里；
//! - 延迟销毁：旧 persist / 旧上传缓冲按 `FrameIndex` 入 `RetireQueue`，完成通知到达后释放（I4）。
//!
//! 三槽 semaphore 保护的是**三槽上传缓冲与 GPU 队列深度**（在飞 command buffer 超过 3 个就会
//! 覆盖别人的实例字节），不是 drawable 池 —— drawable 由 `CAMetalDisplayLink` 提供。
//!
//! 离屏/快照路径仍是同步语义（§4.5 下半段）：`submit_blocking` 在 GPU 线程上等本帧完成，
//! 那里没有 UI 线程可阻塞。

use std::ptr::NonNull;

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

/// 一个命令队列 + 三槽上传背压 + 延迟销毁队列。
pub(crate) struct Queue {
    raw: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    ring: InFlightRing,
    /// 三槽上传缓冲 / GPU 队列深度（Apple「Synchronizing CPU and GPU work」的三缓冲模式）。
    slots: DispatchRetained<DispatchSemaphore>,
    retire: RetireQueue,
    timer: GpuTimer,
}

impl Queue {
    pub(crate) fn new(device: &Device) -> Result<Self, EngineError> {
        let raw = device
            .raw()
            .newCommandQueue()
            .ok_or_else(|| init_error("newCommandQueue", "returned nil"))?;
        Ok(Self {
            raw,
            ring: InFlightRing::new(),
            slots: DispatchSemaphore::new(MAX_FRAMES_IN_FLIGHT as isize),
            retire: RetireQueue::new(),
            timer: GpuTimer,
        })
    }

    /// 非阻塞取一个上传槽；`false` = 三帧在飞，本帧跳帧（帧请求留在最新帧槽里）。
    ///
    /// 成功之后必须由 [`Queue::submit`] 的完成回调 signal，或用 [`Queue::release`] 归还。
    pub(crate) fn try_acquire(&self) -> bool {
        self.slots.wait(DispatchTime::NOW) == 0
    }

    /// 归还一个已经取到、但本帧没有提交的槽位（防御路径，不与 `submit` 同时用）。
    pub(crate) fn release(&self) {
        self.slots.signal();
    }

    /// 取本帧槽位并建一块命令缓冲：**不等待任何 GPU 工作**。
    pub(crate) fn begin(
        &mut self,
        index: FrameIndex,
    ) -> Result<(FrameSlot, Retained<ProtocolObject<dyn MTLCommandBuffer>>), EngineError> {
        let slot = self.ring.acquire(index);
        let buffer = self
            .raw
            .commandBuffer()
            .ok_or_else(|| init_error("commandBuffer", "returned nil"))?;
        Ok((slot, buffer))
    }

    /// 登记完成通知并 `commit`：立即返回（I2）。
    ///
    /// `completed` 在 Metal 的完成回调里执行（任意线程），因此要求 `Send + 'static`；
    /// 回调先 `signal` 三槽 semaphore，再把命令缓冲交给调用方（GPU 线程用它在 drain 时读 GPU 时间）。
    pub(crate) fn submit(
        &mut self,
        slot: FrameSlot,
        buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        completed: impl Fn(Retained<ProtocolObject<dyn MTLCommandBuffer>>) + Send + 'static,
    ) -> Result<(), EngineError> {
        let slots = self.slots.clone();
        let block = RcBlock::new(
            move |buffer: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                slots.signal();
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
        debug_assert_eq!(
            self.ring.last_signaled(),
            slot.signal_value,
            "ring must advance exactly one step per submitted frame"
        );
        Ok(())
    }

    /// 离屏/快照：同步提交（`commit` 后等本帧完成），供 readback 保证内容可读。
    ///
    /// 这是**同步语义**，不是背压：窗口路径永远不经过这里（§4.5 下半段）。
    pub(crate) fn submit_blocking(
        &mut self,
        index: FrameIndex,
        slot: FrameSlot,
        buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
        device: &mut Device,
    ) -> Result<Option<GpuTimerSample>, EngineError> {
        buffer.commit();
        self.ring.advance(slot);
        buffer.waitUntilCompleted();
        self.finish(index, buffer, device)
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

    /// 延迟销毁：本帧用过的资源至少要活到 `index` 的 GPU 工作完成。
    pub(crate) fn retire(&mut self, index: FrameIndex, handle: GpuResourceHandle) {
        self.retire.retire(index, handle);
    }

    /// 完成通知到达（GPU 线程 drain）：读 GPU 时间 + 释放 `retire_at <= index` 的旧资源。
    pub(crate) fn on_completed(
        &mut self,
        index: FrameIndex,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        device: &mut Device,
    ) -> Option<GpuTimerSample> {
        let sample = self.timer.resolve(buffer);
        self.retire.on_completed(index, &mut |handle| {
            device.release(handle);
        });
        sample
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
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
        device: &mut Device,
    ) -> Result<Option<GpuTimerSample>, EngineError> {
        if buffer.status() == MTLCommandBufferStatus::Error {
            return Err(self.failure(index, buffer, device));
        }
        Ok(self.on_completed(index, buffer, device))
    }
}
