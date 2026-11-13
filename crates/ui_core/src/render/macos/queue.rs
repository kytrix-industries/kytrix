//! 命令队列 / 三槽 in-flight / 异步提交 / retire 排空（`design/phase-12-macos-linux.md` §5.2）。
//!
//! **macOS 窗口路径异步提交**：`commit` 后立即返回，不 `waitUntilCompleted`；命令缓冲进
//! `in_flight`，下一次 `submit`/`wait_idle` 轮询状态，完成后推进 retire、归还资源。
//! 主线程因此不再被 GPU 等待占住（AX/全屏转场期间尤其重要）。
//!
//! 离屏快照/readback 仍由 `Renderer` 的 `wait_idle` 保证内容可读。

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

/// 已提交但尚未确认完成的命令缓冲（主线程持有；只在主线程轮询）。
struct Submitted {
    index: FrameIndex,
    buffer: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
}

/// 一个命令队列 + 三槽 in-flight + 延迟销毁队列。
pub(crate) struct Queue {
    raw: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    ring: InFlightRing,
    retire: RetireQueue,
    /// 每个 ring 槽最近一次提交；`None` = 该槽没有在飞帧。
    in_flight: [Option<Submitted>; MAX_FRAMES_IN_FLIGHT],
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
            retire: RetireQueue::new(),
            in_flight: std::array::from_fn(|_| None),
            timer: GpuTimer,
        })
    }

    /// 取本帧槽位并建一块命令缓冲。槽位上一次提交未完成时在这里做**背压等待**。
    pub(crate) fn begin(
        &mut self,
        index: FrameIndex,
        device: &mut Device,
    ) -> Result<(FrameSlot, Retained<ProtocolObject<dyn MTLCommandBuffer>>), EngineError> {
        let slot = self.ring.acquire(index);
        if let Some(previous) = self.in_flight[slot.index].take() {
            previous.buffer.waitUntilCompleted();
            self.finish(previous, device)?;
        }
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

    /// 提交并**立即返回**；完成状态由 [`Queue::poll_completed`] / [`Queue::wait_idle`] 轮询。
    pub(crate) fn submit(
        &mut self,
        index: FrameIndex,
        slot: FrameSlot,
        buffer: &Retained<ProtocolObject<dyn MTLCommandBuffer>>,
    ) -> Result<(), EngineError> {
        buffer.commit();
        self.ring.advance(slot);
        debug_assert_eq!(
            self.ring.last_signaled(),
            slot.signal_value,
            "ring must advance exactly one step per submitted frame"
        );
        self.in_flight[slot.index] = Some(Submitted {
            index,
            buffer: buffer.clone(),
        });
        Ok(())
    }

    /// 等所有在飞帧完成（resize / readback / shutdown）；返回最近一帧的 GPU 采样。
    pub(crate) fn wait_idle(
        &mut self,
        device: &mut Device,
    ) -> Result<Option<GpuTimerSample>, EngineError> {
        let mut last = None;
        for slot in 0..MAX_FRAMES_IN_FLIGHT {
            let Some(submitted) = self.in_flight[slot].take() else {
                continue;
            };
            submitted.buffer.waitUntilCompleted();
            if let Some(sample) = self.finish(submitted, device)? {
                last = Some(sample);
            }
        }
        Ok(last)
    }

    /// 轮询已完成的在飞帧：释放 retire 句柄、取回 GPU 采样；不阻塞。
    pub(crate) fn poll_completed(
        &mut self,
        device: &mut Device,
    ) -> Result<Option<GpuTimerSample>, EngineError> {
        let mut last = None;
        for slot in 0..MAX_FRAMES_IN_FLIGHT {
            let Some(submitted) = self.in_flight[slot].take() else {
                continue;
            };
            match submitted.buffer.status() {
                MTLCommandBufferStatus::Completed => {
                    if let Some(sample) = self.finish(submitted, device)? {
                        last = Some(sample);
                    }
                }
                MTLCommandBufferStatus::Error => {
                    return Err(command_failure(device, submitted.index, &submitted.buffer));
                }
                _ => {
                    // 仍在飞：放回槽位。
                    self.in_flight[slot] = Some(submitted);
                }
            }
        }
        Ok(last)
    }

    /// 延迟销毁：本帧用过的资源至少要活到 `index` 的 GPU 工作完成。
    pub(crate) fn retire(&mut self, index: FrameIndex, handle: GpuResourceHandle) {
        self.retire.retire(index, handle);
    }

    /// 处理一个已完成的命令缓冲：GPU 计时 + retire 释放。
    fn finish(
        &mut self,
        submitted: Submitted,
        device: &mut Device,
    ) -> Result<Option<GpuTimerSample>, EngineError> {
        match submitted.buffer.status() {
            MTLCommandBufferStatus::Completed => {
                let sample = self.timer.resolve(&submitted.buffer);
                self.retire.on_completed(submitted.index, &mut |handle| {
                    device.release(handle);
                });
                Ok(sample)
            }
            MTLCommandBufferStatus::Error => {
                Err(command_failure(device, submitted.index, &submitted.buffer))
            }
            _ => Err(init_error(
                "MTLCommandBufferStatus",
                "buffer did not complete",
            )),
        }
    }
}

/// 命令缓冲失败：先按设备丢失分类（粘性），否则保留 Metal 错误文本。
fn command_failure(
    device: &mut Device,
    index: FrameIndex,
    buffer: &ProtocolObject<dyn MTLCommandBuffer>,
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
