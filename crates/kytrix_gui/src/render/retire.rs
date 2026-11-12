//! 延迟销毁队列（`design/phase-08-d3d12.md` §4.3 / §8；主设计 §2.7 的共享状态机，I4）。
//!
//! 平台无关：只记账「哪些句柄活到哪一帧」。真正的 `drop` 由后端通过 `free` 回调完成
//! （单态泛型，不做 trait object 分派）。

use std::collections::VecDeque;

use crate::window::FrameIndex;

/// 后端定义的资源句柄。
///
/// 编码（**定死**，高位到低位）：`kind:u4 | generation:u28 | index:u32`。
/// - `kind`：1 = 音像对象（`ID3D12Resource`）；2 = descriptor 槽位（P09）；3 = upload arena（P09）；
///   0 与 4..=15 未定义（`ResourceTable::release` 视为编程错误）。
/// - `generation`：槽位复用代数（陈旧句柄被拒绝）。
/// - `index`：后端表的槽位下标。
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

/// 按 `FrameIndex` 分组的延迟销毁队列。
pub(crate) struct RetireQueue {
    /// `(retire_at, handles)`：`retire_at` = 最后一次可能使用该资源的帧。
    frames: VecDeque<(FrameIndex, Vec<GpuResourceHandle>)>,
    completed: FrameIndex,
}

impl RetireQueue {
    pub(crate) fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            completed: FrameIndex::default(),
        }
    }

    /// 在 `at` 帧仍在使用 → 该帧完成前不得释放。
    pub(crate) fn retire(&mut self, at: FrameIndex, handle: GpuResourceHandle) {
        if let Some((last, _)) = self.frames.back()
            && *last == at
        {
            // 同一帧的句柄聚成一批（FIFO 顺序保持）。
            if let Some((_, handles)) = self.frames.back_mut() {
                handles.push(handle);
            }
            return;
        }
        self.frames.push_back((at, vec![handle]));
    }

    /// fence 到达 `completed` 后调用：把 `retire_at <= completed` 的句柄交给 `free`（先入先出）。
    /// 返回本次释放的句柄数。
    pub(crate) fn on_completed(
        &mut self,
        completed: FrameIndex,
        free: &mut impl FnMut(GpuResourceHandle),
    ) -> usize {
        self.completed = completed;
        let mut released = 0;
        while let Some((frame, _)) = self.frames.front() {
            if *frame > completed {
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

    /// 尚未释放的句柄数（测试 / 诊断）。
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.frames.iter().map(|(_, handles)| handles.len()).sum()
    }

    /// 最近一次 `on_completed` 的完成帧。
    #[cfg(test)]
    pub(crate) fn completed(&self) -> FrameIndex {
        self.completed
    }
}

#[cfg(test)]
mod tests {
    use super::{GpuResourceHandle, RetireQueue};
    use crate::window::FrameIndex;

    fn index(value: u32) -> FrameIndex {
        FrameIndex::from_raw(value)
    }

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
    fn nothing_is_released_before_its_frame_completes() {
        let mut queue = RetireQueue::new();
        queue.retire(index(3), handle(1));
        let mut freed = Vec::new();
        assert_eq!(queue.on_completed(index(2), &mut |h| freed.push(h)), 0);
        assert!(freed.is_empty());
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn releases_at_and_before_the_completed_frame() {
        let mut queue = RetireQueue::new();
        queue.retire(index(1), handle(1));
        queue.retire(index(2), handle(2));
        queue.retire(index(3), handle(3));

        let mut freed = Vec::new();
        assert_eq!(queue.on_completed(index(2), &mut |h| freed.push(h)), 2);
        assert_eq!(freed, vec![handle(1), handle(2)]);
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn handles_at_the_same_frame_are_batched_in_order() {
        let mut queue = RetireQueue::new();
        queue.retire(index(5), handle(7));
        queue.retire(index(5), handle(8));
        assert_eq!(queue.pending(), 2);

        let mut freed = Vec::new();
        queue.on_completed(index(5), &mut |h| freed.push(h));
        assert_eq!(freed, vec![handle(7), handle(8)]);
        assert_eq!(queue.pending(), 0);
        assert_eq!(queue.completed(), index(5));
    }

    #[test]
    fn empty_queue_is_stable() {
        let mut queue = RetireQueue::new();
        assert_eq!(queue.pending(), 0);
        assert_eq!(queue.on_completed(index(1), &mut |_| unreachable!()), 0);
        assert_eq!(queue.pending(), 0);
    }
}
