//! in-flight 帧轮转（`design/phase-08-d3d12.md` §4.2）。
//!
//! `FrameIndex` 本身留在 `window.rs`（P06 的公开 API 返回类型，DEVIATIONS F-74）；本模块只提供
//! 「槽位 + 需要等待/将要 signal 的 fence 值」，由 `render/windows/queue.rs` 消费。

use crate::window::FrameIndex;

/// in-flight 帧数（主设计 §2.4；同步提交下实际最多 1 帧在飞，但 ring 结构不依赖这一点）。
pub(crate) const MAX_FRAMES_IN_FLIGHT: usize = 3;

/// 一帧要用的槽位与 fence 值。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameSlot {
    /// ring 槽位（`0..MAX_FRAMES_IN_FLIGHT`）。
    pub index: usize,
    /// 进入本帧前必须等待到达的 fence 值（0 = 该槽从未使用，无需等待）。
    pub wait_value: u64,
    /// 本帧提交成功后 signal 的值。
    pub signal_value: u64,
}

/// ring 的 fence 记账（唯一状态是「每个槽最后一次提交的值」）。
pub(crate) struct InFlightRing {
    slot_values: [u64; MAX_FRAMES_IN_FLIGHT],
    next_value: u64,
}

impl InFlightRing {
    pub(crate) const fn new() -> Self {
        Self {
            slot_values: [0; MAX_FRAMES_IN_FLIGHT],
            next_value: 0,
        }
    }

    /// 取本帧槽位（不修改状态；提交成功后才 [`InFlightRing::advance`]）。
    pub(crate) fn acquire(&self, index: FrameIndex) -> FrameSlot {
        let slot = slot_of(index);
        FrameSlot {
            index: slot,
            wait_value: self.slot_values[slot],
            signal_value: self.next_value + 1,
        }
    }

    /// 提交成功后推进该槽的 fence 值。
    pub(crate) fn advance(&mut self, slot: FrameSlot) {
        self.slot_values[slot.index] = slot.signal_value;
        self.next_value = slot.signal_value;
    }

    /// 最后一次 signal 的值（诊断 / `wait_idle`）。
    pub(crate) fn last_signaled(&self) -> u64 {
        self.next_value
    }

    /// 给「帧外一次性提交」（readback / resize / 图集上传）预留一个 fence 值。
    ///
    /// 只推进 `next_value`、不动槽位：后续真正帧的 signal 值继续单调递增。
    ///
    /// 唯一调用方是 Windows 的 `Queue::submit_immediate`；macOS/Linux 后端没有帧外提交，
    /// 因此这层 `cfg` 必须同时包含 `windows`，否则非 Windows 目标下是 `dead_code`
    /// （`--all-features` 也会把 `test-support` 打开）。
    #[cfg(all(windows, any(test, feature = "test-support")))]
    pub(crate) fn reserve(&mut self) -> u64 {
        self.next_value += 1;
        self.next_value
    }
}

/// `FrameIndex` → ring 槽位。`0` 是「从未提交」的非法值（`FrameScheduler::commit` 保证 ≥ 1）。
fn slot_of(index: FrameIndex) -> usize {
    debug_assert!(index.get() >= 1, "FrameIndex 0 从未提交");
    (index.get() as usize) % MAX_FRAMES_IN_FLIGHT
}

#[cfg(test)]
mod tests {
    use super::{FrameSlot, InFlightRing, MAX_FRAMES_IN_FLIGHT};
    use crate::window::FrameIndex;

    fn index(value: u32) -> FrameIndex {
        // `FrameIndex` 的字段是私有的；测试用 `Default` + 提交语义之外的路径不可用，
        // 因此这里通过 `FrameIndex` 的公开构造面（同一 crate 内）走 `From` 语义的替身。
        FrameIndex::from_raw(value)
    }

    #[test]
    fn slots_cycle_through_the_ring() {
        let ring = InFlightRing::new();
        let slots: Vec<usize> = (1..=7)
            .map(|value| ring.acquire(index(value)).index)
            .collect();
        assert_eq!(slots, vec![1, 2, 0, 1, 2, 0, 1]);
        assert_eq!(slots.len(), 7);
        assert_eq!(MAX_FRAMES_IN_FLIGHT, 3);
    }

    #[test]
    fn first_use_of_each_slot_has_no_wait() {
        let mut ring = InFlightRing::new();
        for value in 1..=3 {
            let slot = ring.acquire(index(value));
            assert_eq!(slot.wait_value, 0, "slot {} must not wait", slot.index);
            ring.advance(slot);
        }
    }

    #[test]
    fn wait_value_is_the_previous_submission_of_that_slot() {
        let mut ring = InFlightRing::new();
        let first = ring.acquire(index(1));
        ring.advance(first);
        let second = ring.acquire(index(2));
        ring.advance(second);
        let third = ring.acquire(index(3));
        ring.advance(third);

        // frame 4 落在 slot 1（frame 1 的槽），必须等 frame 1 的 signal 值。
        let fourth = ring.acquire(index(4));
        assert_eq!(fourth.index, 1);
        assert_eq!(fourth.wait_value, first.signal_value);
        assert_eq!(fourth.signal_value, 4);
    }

    #[test]
    fn signal_values_are_monotonic() {
        let mut ring = InFlightRing::new();
        let mut previous = 0;
        for value in 1..=9 {
            let slot = ring.acquire(index(value));
            assert_eq!(slot.signal_value, previous + 1);
            previous = slot.signal_value;
            ring.advance(slot);
        }
        assert_eq!(ring.last_signaled(), 9);
    }

    #[test]
    fn advance_only_touches_its_own_slot() {
        let mut ring = InFlightRing::new();
        let slot = ring.acquire(index(2));
        ring.advance(FrameSlot {
            index: 2,
            wait_value: 0,
            signal_value: 41,
        });
        let again = ring.acquire(index(2));
        assert_eq!(again.wait_value, 41);
        assert_eq!(ring.acquire(index(3)).wait_value, 0);
        let _ = slot;
    }
}
