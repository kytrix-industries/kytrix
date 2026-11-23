use crate::window::FrameIndex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Section {
    Upload,
    Gpu,
    Present,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GpuTimerSample {
    pub section: Section,
    pub cpu_ns: u64,
    pub gpu_ns: u64,
}

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

    #[cfg(feature = "hud")]
    pub(crate) fn sample(&self, section: Section) -> Option<GpuTimerSample> {
        self.samples
            .iter()
            .find(|sample| sample.section == section)
            .copied()
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

    #[cfg(feature = "hud")]
    #[test]
    fn sample_returns_the_first_matching_section() {
        let mut timings = FrameTimings::new();
        timings.reset(FrameIndex::from_raw(1));
        assert_eq!(timings.sample(Section::Upload), None);
        timings.push(GpuTimerSample {
            section: Section::Upload,
            cpu_ns: 10,
            gpu_ns: 0,
        });
        timings.push(GpuTimerSample {
            section: Section::Gpu,
            cpu_ns: 0,
            gpu_ns: 20,
        });
        assert_eq!(timings.sample(Section::Upload).map(|s| s.cpu_ns), Some(10));
        assert_eq!(timings.sample(Section::Gpu).map(|s| s.gpu_ns), Some(20));
        assert_eq!(timings.sample(Section::Present), None);
    }
}
