//! 延迟销毁队列（`design/phase-08-d3d12.md` §4.3 / §8；主设计 §2.7 的共享状态机，I4）。
//!
//! 平台无关：只记账「哪些句柄活到哪一帧」。真正的 `drop` 由后端通过 `free` 回调完成
//! （单态泛型，不做 trait object 分派）。
//!
//! **键是 `u64` 不是 `FrameIndex`**（R06-1 / R11-1）：`FrameIndex` 是 UI 侧的 `u32` 提交序号，
//! 60 fps 约 2.27 年、120 fps 约 1.13 年就会饱和；饱和后同一帧号代表所有后续帧，队列会退化为
//! 「立刻释放」。Vulkan 的后端已经有一条自己的 `u64` 提交序号（timeline 的 signal 值，见
//! `render::frame::InFlightRing`），retire 必须按它记账，而不是按 UI 帧号。Windows / macOS 仍按
//! `FrameIndex`（D3D12 的 fence 值同样来自提交序号，两者当前一致），因此键用
//! [`RetireKey`] 泛化：两种键都直接比较大小，不需要调用方转换或新增状态。

use std::collections::VecDeque;

use crate::window::FrameIndex;

/// retire 队列的键：`u64` 提交序号或 UI 侧的 [`FrameIndex`]。
///
/// 存在的唯一理由：Linux 的完成判据是 timeline 的 `u64` signal 值（提交序号），Windows / macOS
/// 用 `FrameIndex`。两者都是「单调递增的完成点」，队列只需要能比较大小。
pub(crate) trait RetireKey: Copy + Ord {
    /// 原始键值（诊断 / 测试）。
    fn key(self) -> u64;
}

impl RetireKey for u64 {
    fn key(self) -> u64 {
        self
    }
}

impl RetireKey for FrameIndex {
    fn key(self) -> u64 {
        u64::from(self.get())
    }
}

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

/// 按完成点（提交序号）分组的延迟销毁队列。
pub(crate) struct RetireQueue {
    /// `(retire_at, handles)`：`retire_at` = 最后一次可能使用该资源的提交序号。
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

    /// 在 `at` 仍在使用 → 该提交完成前不得释放。
    pub(crate) fn retire(&mut self, at: impl RetireKey, handle: GpuResourceHandle) {
        let at = at.key();
        if let Some((last, _)) = self.frames.back()
            && *last == at
        {
            // 同一次提交的句柄聚成一批（FIFO 顺序保持）。
            if let Some((_, handles)) = self.frames.back_mut() {
                handles.push(handle);
            }
            return;
        }
        self.frames.push_back((at, vec![handle]));
    }

    /// 完成点到达 `completed` 后调用：把 `retire_at <= completed` 的句柄交给 `free`（先入先出）。
    /// 返回本次释放的句柄数。
    pub(crate) fn on_completed(
        &mut self,
        completed: impl RetireKey,
        free: &mut impl FnMut(GpuResourceHandle),
    ) -> usize {
        self.completed = completed.key();
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

    /// 丢弃全部待释放句柄（teardown：调用方保证 GPU 已空闲）。返回释放的句柄数。
    ///
    /// 正常路径只应由 [`RetireQueue::on_completed`] 推进：没提交的帧（错误路径）永远不会
    /// 完成，它们的句柄只能在 teardown 一次性释放。目前只有 Linux 的 `Device::drop` 调用它。
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

    /// 尚未释放的句柄数（测试 / 诊断）。
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.frames.iter().map(|(_, handles)| handles.len()).sum()
    }

    /// 最近一次 `on_completed` 的完成点（原始键值）。
    #[cfg(test)]
    pub(crate) fn completed(&self) -> u64 {
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
        assert_eq!(queue.completed(), 5, "完成点记的是原始键值");
    }

    #[test]
    fn skipped_frame_indices_do_not_block_retirement() {
        // 提交序号 1、3、5（2、4 被跳过：没有 GPU 工作，也就不会出现在完成通知里）。
        let mut queue = RetireQueue::new();
        for frame in [1, 3, 5] {
            queue.retire(index(frame), handle(frame));
        }

        // 完成通知只报「真正提交过」的最大序号 4（§4.3：completed 直接跨过被跳过的序号）。
        let mut freed = Vec::new();
        assert_eq!(queue.on_completed(index(4), &mut |h| freed.push(h)), 2);
        assert_eq!(freed, vec![handle(1), handle(3)]);
        assert_eq!(queue.pending(), 1, "5 的句柄不得提前释放");

        assert_eq!(queue.on_completed(index(5), &mut |h| freed.push(h)), 1);
        assert_eq!(freed, vec![handle(1), handle(3), handle(5)]);
        assert_eq!(queue.pending(), 0);
    }

    /// R06-1 / R11-1：Linux 用 `u64` 提交序号（timeline 的 signal 值）记账，**与 `FrameIndex`
    /// 无关**。这里提交序号取 `u64::MAX` 附近：`FrameIndex` 在这个量级早已饱和（全帧同号），
    /// 而队列仍必须按序号逐次释放（饱和会让「按帧延迟释放」退化成「立刻释放」）。
    #[test]
    fn u64_sequences_retire_independently_of_frame_indices() {
        let mut queue = RetireQueue::new();
        let base = u64::MAX - 3;
        for offset in 0..3u64 {
            queue.retire(base + offset, handle(offset as u32));
        }
        let mut freed = Vec::new();
        // 完成到倒数第二个提交：只释放前两个。
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

    /// 两种键共用同一套 FIFO / 批量规则：混合提交（先 `FrameIndex` 后 `u64`）也按值排序，
    /// 后端切换记账来源不会破坏「先入先出」。
    #[test]
    fn both_key_kinds_share_the_same_ordering_rules() {
        let mut queue = RetireQueue::new();
        queue.retire(index(2), handle(1));
        queue.retire(3u64, handle(2));
        queue.retire(index(4), handle(3));
        assert_eq!(queue.pending(), 3);

        let mut freed = Vec::new();
        assert_eq!(queue.on_completed(3u64, &mut |h| freed.push(h)), 2);
        assert_eq!(freed, vec![handle(1), handle(2)]);
        assert_eq!(queue.pending(), 1);
    }

    #[test]
    fn empty_queue_is_stable() {
        let mut queue = RetireQueue::new();
        assert_eq!(queue.pending(), 0);
        assert_eq!(queue.on_completed(index(1), &mut |_| unreachable!()), 0);
        assert_eq!(queue.pending(), 0);
    }
}
