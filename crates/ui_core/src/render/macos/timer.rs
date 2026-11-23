use objc2::runtime::ProtocolObject;
use objc2_metal::MTLCommandBuffer;

use crate::render::timer::{GpuTimerSample, Section};

#[derive(Debug, Default)]
pub(crate) struct GpuTimer;

impl GpuTimer {
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
