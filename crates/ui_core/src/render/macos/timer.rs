//! GPU 计时：`MTLCommandBuffer::GPUStartTime` / `GPUEndTime`（`design/phase-12-macos-linux.md` §5.8）。
//!
//! 不用 `MTLCounterSampleBuffer`（`counterSets` 可能为空、要拆 pass、CI 无法验证）；command buffer 级
//! 的 GPU 时间足以喂 `Section::Gpu`（F-131）。

use objc2::runtime::ProtocolObject;
use objc2_metal::MTLCommandBuffer;

use crate::render::timer::{GpuTimerSample, Section};

/// 无状态：每帧的 GPU 时间来自该帧的命令缓冲自身。
#[derive(Debug, Default)]
pub(crate) struct GpuTimer;

impl GpuTimer {
    /// 恒可用（Metal 的 command buffer 一定有起止时间）。
    pub(crate) fn new() -> Option<Self> {
        Some(Self)
    }

    /// 命令缓冲完成后调用：`(GPUStartTime, GPUEndTime)` 秒 → 纳秒。
    pub(crate) fn resolve(
        &self,
        buffer: &ProtocolObject<dyn MTLCommandBuffer>,
    ) -> Option<GpuTimerSample> {
        let start = buffer.GPUStartTime();
        let end = buffer.GPUEndTime();
        if !(start > 0.0 && end > start) {
            return None;
        }
        let gpu_ns = ((end - start) * 1e9) as u64;
        Some(GpuTimerSample {
            section: Section::Gpu,
            cpu_ns: 0,
            gpu_ns,
        })
    }
}
