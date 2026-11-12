//! 分段时间（`design/phase-08-d3d12.md` §4.5 / §5.8；主设计 §2.10 的数值语义）。
//!
//! P08 有生产者的段只有 `Upload` / `Gpu` / `Present`；`Build`/`Layout`/`Shape`/`Paint` 属
//! P09 的 App 级计时，届时 additive 追加。

use crate::window::FrameIndex;

/// 有生产者的一段。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Section {
    /// 实例/裁剪/图集上传的 CPU 时间。
    Upload,
    /// GPU 执行时间（timestamp query）。
    Gpu,
    /// `Present1` 的 CPU 时间。
    Present,
}

/// 一段的采样；`gpu_ns` 只在 [`Section::Gpu`] 上有值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GpuTimerSample {
    pub section: Section,
    pub cpu_ns: u64,
    pub gpu_ns: u64,
}

/// 一帧的采样集合（容量复用；与 `FrameIndex` 对齐）。
pub(crate) struct FrameTimings {
    frame: FrameIndex,
    samples: Vec<GpuTimerSample>,
}

impl FrameTimings {
    pub(crate) fn new() -> Self {
        Self {
            frame: FrameIndex::default(),
            samples: Vec::with_capacity(3),
        }
    }

    /// 帧初重置（保留容量）。
    pub(crate) fn reset(&mut self, frame: FrameIndex) {
        self.frame = frame;
        self.samples.clear();
    }

    pub(crate) fn push(&mut self, sample: GpuTimerSample) {
        self.samples.push(sample);
    }

    pub(crate) fn samples(&self) -> &[GpuTimerSample] {
        &self.samples
    }

    pub(crate) fn frame(&self) -> FrameIndex {
        self.frame
    }
}

#[cfg(test)]
mod tests {
    use super::{FrameTimings, GpuTimerSample, Section};
    use crate::window::FrameIndex;

    #[test]
    fn reset_clears_samples_but_keeps_capacity() {
        let mut timings = FrameTimings::new();
        timings.reset(FrameIndex::from_raw(1));
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: 10,
            gpu_ns: 0,
        });
        let capacity = timings.samples.capacity();
        assert_eq!(timings.samples().len(), 1);

        timings.reset(FrameIndex::from_raw(2));
        assert!(timings.samples().is_empty());
        assert_eq!(timings.frame(), FrameIndex::from_raw(2));
        assert_eq!(timings.samples.capacity(), capacity);
    }

    #[test]
    fn sections_have_a_stable_order() {
        assert!(Section::Upload < Section::Gpu);
        assert!(Section::Gpu < Section::Present);
    }
}
