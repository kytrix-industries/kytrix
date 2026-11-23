use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct GpuResourceHandle(u64);

impl GpuResourceHandle {
    pub(crate) const fn new(kind: u8, generation: u32, index: u32) -> Self {
        debug_assert!(kind < 16, "GpuResourceHandle kind must fit in 4 bits");
        debug_assert!(generation < (1 << 28), "generation must fit in 28 bits");
        Self(((kind as u64) << 60) | ((generation as u64) << 32) | (index as u64))
    }

    pub(crate) const fn kind(self) -> u8 {
        (self.0 >> 60) as u8
    }

    pub(crate) const fn generation(self) -> u32 {
        ((self.0 >> 32) & 0x0FFF_FFFF) as u32
    }

    pub(crate) const fn index(self) -> u32 {
        self.0 as u32
    }
}

pub(crate) struct RetireQueue {
    frames: VecDeque<(u64, Vec<GpuResourceHandle>)>,
    completed: u64,
}

impl RetireQueue {
    pub(crate) fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            completed: 0,
        }
    }

    pub(crate) fn retire(&mut self, at: u64, handle: GpuResourceHandle) {
        if let Some((last, _)) = self.frames.back()
            && *last == at
        {
            if let Some((_, handles)) = self.frames.back_mut() {
                handles.push(handle);
            }
            return;
        }
        self.frames.push_back((at, vec![handle]));
    }

    pub(crate) fn on_completed(
        &mut self,
        completed: u64,
        free: &mut impl FnMut(GpuResourceHandle),
    ) -> usize {
        self.completed = completed;
        let mut released = 0;
        while let Some((frame, _)) = self.frames.front() {
            if *frame > self.completed {
                break;
            }
            let (_, handles) = self.frames.pop_front().expect("front checked above");
            for handle in handles {
                free(handle);
                released += 1;
            }
        }
        released
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn drain_all(&mut self, free: &mut impl FnMut(GpuResourceHandle)) -> usize {
        let mut released = 0;
        while let Some((_, handles)) = self.frames.pop_front() {
            for handle in handles {
                free(handle);
                released += 1;
            }
        }
        released
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.frames.iter().map(|(_, handles)| handles.len()).sum()
    }

    #[cfg(test)]
    pub(crate) fn completed(&self) -> u64 {
        self.completed
    }
}

#[cfg(test)]
mod tests {
    use super::{GpuResourceHandle, RetireQueue};
    use crate::window::FrameIndex;

    fn handle(index: u32) -> GpuResourceHandle {
        GpuResourceHandle::new(1, index % 4, index)
    }

    #[test]
    fn handle_round_trips_its_three_fields() {
        let value = GpuResourceHandle::new(1, 0x0ABC_DEF0, 0x1234_5678);
        assert_eq!(value.kind(), 1);
        assert_eq!(value.generation(), 0x0ABC_DEF0);
        assert_eq!(value.index(), 0x1234_5678);
    }

    #[test]
    fn nothing_is_released_before_its_submission_completes() {
        let mut queue = RetireQueue::new();
        queue.retire(3, handle(1));
        let mut freed = Vec::new();
        assert_eq!(queue.on_completed(2, &mut |h| freed.push(h)), 0);
        assert!(freed.is_empty());
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn releases_at_and_before_the_completed_sequence() {
        let mut queue = RetireQueue::new();
        queue.retire(1, handle(1));
        queue.retire(2, handle(2));
        queue.retire(3, handle(3));

        let mut freed = Vec::new();
        assert_eq!(queue.on_completed(2, &mut |h| freed.push(h)), 2);
        assert_eq!(freed, vec![handle(1), handle(2)]);
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn handles_at_the_same_commit_point_are_batched_in_order() {
        let mut queue = RetireQueue::new();
        queue.retire(5, handle(7));
        queue.retire(5, handle(8));
        assert_eq!(queue.pending(), 2);

        let mut freed = Vec::new();
        queue.on_completed(5, &mut |h| freed.push(h));
        assert_eq!(freed, vec![handle(7), handle(8)]);
        assert_eq!(queue.pending(), 0);
        assert_eq!(queue.completed(), 5, "完成点记的是提交序号本身");
    }

    #[test]
    fn skipped_sequences_do_not_block_retirement() {
        let mut queue = RetireQueue::new();
        for sequence in [1, 3, 5] {
            queue.retire(sequence, handle(sequence as u32));
        }

        let mut freed = Vec::new();
        assert_eq!(queue.on_completed(4, &mut |h| freed.push(h)), 2);
        assert_eq!(freed, vec![handle(1), handle(3)]);
        assert_eq!(queue.pending(), 1, "5 的句柄不得提前释放");

        assert_eq!(queue.on_completed(5, &mut |h| freed.push(h)), 1);
        assert_eq!(freed, vec![handle(1), handle(3), handle(5)]);
        assert_eq!(queue.pending(), 0);
    }

    #[test]
    fn sequences_near_u64_max_retire_in_submission_order() {
        let mut queue = RetireQueue::new();
        let base = u64::MAX - 3;
        for offset in 0..3u64 {
            queue.retire(base + offset, handle(offset as u32));
        }
        let mut freed = Vec::new();
        assert_eq!(
            queue.on_completed(base + 1, &mut |h| freed.push(h)),
            2,
            "retire 必须按 u64 提交序号逐次推进"
        );
        assert_eq!(freed, vec![handle(0), handle(1)]);
        assert_eq!(queue.pending(), 1, "最后一个提交的句柄不得提前释放");
        assert_eq!(queue.completed(), base + 1);

        assert_eq!(queue.on_completed(base + 2, &mut |h| freed.push(h)), 1);
        assert_eq!(freed, vec![handle(0), handle(1), handle(2)]);
        assert_eq!(queue.pending(), 0);
    }

    #[test]
    fn saturated_frame_indices_do_not_degrade_retirement() {
        let mut queue = RetireQueue::new();
        let saturated = FrameIndex::from_raw(u32::MAX);
        assert_eq!(saturated.get(), u32::MAX);
        let base = u64::from(u32::MAX) + 1;
        for offset in 0..3u64 {
            queue.retire(base + offset, handle(offset as u32));
        }

        let mut freed = Vec::new();
        assert_eq!(
            queue.on_completed(base, &mut |h| freed.push(h)),
            1,
            "饱和帧号不能把三个提交点压成一个"
        );
        assert_eq!(freed, vec![handle(0)]);
        assert_eq!(queue.pending(), 2, "后两个提交的句柄仍在飞");
        assert_eq!(queue.on_completed(base + 1, &mut |h| freed.push(h)), 1);
        assert_eq!(queue.on_completed(base + 2, &mut |h| freed.push(h)), 1);
        assert_eq!(freed, vec![handle(0), handle(1), handle(2)]);
        assert_eq!(queue.pending(), 0);
    }

    #[test]
    fn empty_queue_is_stable() {
        let mut queue = RetireQueue::new();
        assert_eq!(queue.pending(), 0);
        assert_eq!(queue.on_completed(1, &mut |_| unreachable!()), 0);
        assert_eq!(queue.pending(), 0);
    }
}
