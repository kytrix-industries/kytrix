//! 命令队列 / 三槽 in-flight / 同步提交 / retire 排空（`design/phase-12-macos-linux.md` §5.2）。
//!
//! **同步提交**（与 D3D12 的「Signal + 等 fence」同构）：`commit` → `waitUntilCompleted`。
//! `MTLSharedEvent` 不进 v1（`encodeWaitForEvent:` 需要 objc2 没有的协议上转，F-120）。

use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_metal::{MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandQueue, MTLDevice};

use crate::{
    error::EngineError,
    render::{
        frame::{FrameSlot, InFlightRing},
        macos::device::{Device, init_error},
        retire::{GpuResourceHandle, RetireQueue},
    },
    window::FrameIndex,
};

/// 一个命令队列 + 三槽 fence 记账 + 延迟销毁队列。
pub(crate) struct Queue {
    raw: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    ring: InFlightRing,
    retire: RetireQueue,
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
            retire: RetireQueue::new(),
        })
    }

    /// 取本帧槽位并建一块命令缓冲（Metal 的命令缓冲是短命对象，不跨帧复用）。
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

    /// 帧外的一次性命令缓冲（readback / 图集重建）：**不**动 ring 的槽位。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn command_buffer(
        &self,
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, EngineError> {
        self.raw
            .commandBuffer()
            .ok_or_else(|| init_error("commandBuffer", "returned nil"))
    }

    /// 提交并同步等待：`commit` → `waitUntilCompleted` → `status` 判定 → 推进 ring。
    ///
    /// 返回 `Err` 时**不**推进 ring（下一次同槽会等同一个值，但 Metal 没有 fence 计数器，
    /// 因此这里只在成功路径记账；失败帧的 `frame_pending` 由 `Renderer` 的粘性错误收口）。
    pub(crate) fn submit(
        &mut self,
        slot: FrameSlot,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) -> Result<(), EngineError> {
        buffer.commit();
        buffer.waitUntilCompleted();
        match buffer.status() {
            MTLCommandBufferStatus::Completed => {
                debug_assert_eq!(
                    self.ring.last_signaled(),
                    slot.signal_value.saturating_sub(1),
                    "ring must advance exactly one step per submitted frame"
                );
                self.ring.advance(slot);
                Ok(())
            }
            MTLCommandBufferStatus::Error => Err(EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "MTLCommandBuffer failed: {}",
                    buffer
                        .error()
                        .map(|error| error.localizedDescription().to_string())
                        .unwrap_or_else(|| "unknown MTLCommandBufferError".to_owned())
                ),
            }),
            MTLCommandBufferStatus::NotEnqueued
            | MTLCommandBufferStatus::Enqueued
            | MTLCommandBufferStatus::Scheduled
            | MTLCommandBufferStatus::Committed => Err(init_error(
                "MTLCommandBufferStatus",
                "buffer did not complete",
            )),
            _ => Err(init_error(
                "MTLCommandBufferStatus",
                "unknown command buffer status",
            )),
        }
    }

    /// 等最后一次提交完成（resize / 快照 / 退出）。同步提交下恒为 no-op。
    pub(crate) fn wait_idle(&mut self) {}

    /// 延迟销毁：本帧用过的资源至少要活到 `index` 的 GPU 工作完成。
    pub(crate) fn retire(&mut self, index: FrameIndex, handle: GpuResourceHandle) {
        self.retire.retire(index, handle);
    }

    /// 把 `completed` 及更早的句柄交回 `device`（真正 drop `Retained`）。
    pub(crate) fn release_completed(
        &mut self,
        completed: FrameIndex,
        device: &mut Device,
    ) -> usize {
        self.retire.on_completed(completed, &mut |handle| {
            device.release(handle);
        })
    }
}
