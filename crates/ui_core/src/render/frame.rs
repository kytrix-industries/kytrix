//! 帧交付契约（共享，纯数据）：`FramePayload` / `FrameRequest` / `FrameFeedback` 与最新帧槽；
//! 以及 in-flight 帧轮转（`design/phase-08-d3d12.md` §4.2）。
//!
//! 本模块是**三平台共用**的纯数据 + 纯函数（`design/async-render-pipeline.md` §4.1 / §4.2）：
//! 不含线程、句柄或任何平台 API。UI 线程产帧、GPU 线程执行，跨线程只搬这里的值。
//! 线程原语（Windows 的 work event、Linux 的 condvar、macOS 的 display link）各自留在平台模块内。
//!
//! `FrameIndex` 本身留在 `window.rs`（P06 的公开 API 返回类型，DEVIATIONS F-74）；本模块只提供
//! 「槽位 + 需要等待/将要 signal 的 fence 值」（`render/windows/queue.rs` 消费）与帧交付数据。

use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::{
    paint::{
        instance::{FrameOp, GpuClipEntry, InstanceBuffer},
        primitive::GeometryDraw,
    },
    render::{compositor::Plan, stats::RenderStats, timer::GpuTimerSample},
    window::FrameIndex,
};

/// 一帧的 CPU→GPU 快照（`design/async-render-pipeline.md` §4.1）。
///
/// 实例字节、绘制序、裁剪表、几何表必须一起过线：GPU 线程拿到的必须是**同一帧**的四份数据。
/// 每帧一次拷贝是线程边界的所有权转移，不是可选优化（§4.2 的代价说明）。
pub(crate) struct FramePayload {
    instances: Bytes,
    ops: Vec<FrameOp>,
    clips: Vec<GpuClipEntry>,
    /// 几何绘制表：`FrameOp::Geometry.index` 的下标表，**必须与 `ops` 同帧过线**
    /// （`ops` 里的下标只有配合同一帧的表才有意义）。
    ///
    /// 目前只有 Windows 的录制路径消费它（macOS / Linux 的 `FrameOp::Geometry` 还没有 GPU
    /// 缓冲来源），但三平台的 payload 形状保持一致：它是「一帧的全部 CPU 侧数据」，不是按
    /// 平台裁剪的数据包。
    geometry: Vec<GeometryDraw>,
}

impl FramePayload {
    /// 从 UI 线程的编码结果复制一份。
    pub(crate) fn from_buffer(buffer: &InstanceBuffer) -> Self {
        let mut clips = Vec::with_capacity(buffer.clips().len());
        buffer.write_clips_gpu(&mut clips);
        Self {
            instances: Bytes::copy_from_slice(buffer.as_bytes()),
            ops: buffer.ops().to_vec(),
            clips,
            geometry: buffer.geometry().to_vec(),
        }
    }

    /// 本帧的实例字节（`GpuPrimitive` 数组，128 B 布局）。
    pub(crate) fn instances(&self) -> &[u8] {
        &self.instances
    }

    /// 本帧的绘制序。
    pub(crate) fn ops(&self) -> &[FrameOp] {
        &self.ops
    }

    /// 本帧的裁剪表（GPU 布局，表项 0 是哨兵）。
    pub(crate) fn clips(&self) -> &[GpuClipEntry] {
        &self.clips
    }

    /// `FrameOp::Geometry.index` 的下标表（与 [`FramePayload::ops`] 同帧）。
    pub(crate) fn geometry(&self) -> &[GeometryDraw] {
        &self.geometry
    }

    /// 空载荷（槽位/线程单元测试用；真实帧必须走 [`FramePayload::from_buffer`]）。
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            instances: Bytes::new(),
            ops: Vec::new(),
            clips: Vec::new(),
            geometry: Vec::new(),
        }
    }
}

/// UI 线程 → GPU 线程的一帧请求（§4.1）。
///
/// `index` 是 UI 侧的提交序号；GPU 线程只对真正录制并提交的帧推进自己的记账（§4.3），
/// 被跳过的序号不产生 GPU 工作，`FrameIndex` 因此不要求连续出现在完成通知里。
pub(crate) struct FrameRequest {
    pub index: FrameIndex,
    pub payload: FramePayload,
    /// UI 侧的 damage 计划；GPU 线程在渲染前与自己的「目标是新建/重建」状态合成 `FramePlan`。
    pub plan: Plan,
    /// 目标物理像素（resize 的唯一来源，§4.4）。
    pub surface: (u32, u32),
    /// HUD 的发布延迟基准（[`FrameFeedback::latency`] 的起点）。
    pub published_at: Instant,
}

/// GPU 线程 → UI 线程的一帧反馈（§4.1）。
///
/// 窗口路径的 `RenderStats` / GPU 采样从这里取（落后 ≤1 帧）；离屏路径保持同步返回。
///
/// **跨平台契约**：GPU 线程必须在**观察到**该帧的 GPU 工作完成之后才发布反馈（fence /
/// timeline / completed handler），不得在只是「提交了」的时候发布；反馈按提交顺序发布；
/// 允许覆盖尚未被 UI 线程取走的上一条（反馈槽是最新者胜，不是队列）。
pub(crate) struct FrameFeedback {
    pub index: FrameIndex,
    pub stats: RenderStats,
    pub samples: Vec<GpuTimerSample>,
    /// GPU 线程**观察到该帧 GPU 工作完成**的时刻（fence / timeline / completed handler），
    /// 不是 `Present` 返回的时刻。
    pub completed_at: Instant,
    /// `completed_at - published_at`：UI 线程发布本帧到 GPU 完成的时长（HUD 的
    /// publish→present 一栏）。起点是 [`FrameRequest::published_at`]。
    pub latency: Duration,
}

/// 最新帧槽：**最新者胜 + damage 合并**（§4.2 / I5）。
///
/// 这不是队列：GPU 线程忙时后来的帧覆盖先前的帧，被覆盖帧的 damage 由后继帧补画
/// （persist 纹理跨帧保留，被跳过的帧没有写进去）。因此全程没有「强制全量」的补救路径。
/// 锁与唤醒原语不在这里：本类型是纯数据，各平台门面自行决定同步方式。
pub(crate) struct LatestFrame {
    pending: Option<FrameRequest>,
}

impl LatestFrame {
    pub(crate) const fn new() -> Self {
        Self { pending: None }
    }

    /// 放入一帧；已有未取走的帧时保留**新**的 payload/surface，把旧的 damage 合并进新的
    /// plan（[`Plan::merge`]），旧请求的 `index` 丢弃 —— 它不会产生任何 GPU 工作。
    pub(crate) fn publish(&mut self, request: FrameRequest) {
        let Some(previous) = self.pending.as_mut() else {
            self.pending = Some(request);
            return;
        };
        let mut next = request;
        next.plan = previous.plan.merge(next.plan);
        *previous = next;
    }

    /// 取走当前帧（没有则 `None`）。
    pub(crate) fn take(&mut self) -> Option<FrameRequest> {
        self.pending.take()
    }

    /// 是否有未取走的帧。
    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

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
}

/// `FrameIndex` → ring 槽位。`0` 是「从未提交」的非法值（`FrameScheduler::commit` 保证 ≥ 1）。
fn slot_of(index: FrameIndex) -> usize {
    debug_assert!(index.get() >= 1, "FrameIndex 0 从未提交");
    (index.get() as usize) % MAX_FRAMES_IN_FLIGHT
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{
        FramePayload, FrameRequest, FrameSlot, InFlightRing, LatestFrame, MAX_FRAMES_IN_FLIGHT,
    };
    use crate::{
        math::PhysicalRect,
        render::compositor::{DamageRects, Plan},
        window::FrameIndex,
    };

    fn index(value: u32) -> FrameIndex {
        // `FrameIndex` 的字段是私有的；测试用 `Default` + 提交语义之外的路径不可用，
        // 因此这里通过 `FrameIndex` 的公开构造面（同一 crate 内）走 `From` 语义的替身。
        FrameIndex::from_raw(value)
    }

    fn partial(rects: &[PhysicalRect]) -> Plan {
        let mut plan = DamageRects::new();
        for rect in rects {
            assert!(plan.push(*rect));
        }
        Plan::Partial(plan)
    }

    fn request(index: u32, plan: Plan) -> FrameRequest {
        FrameRequest {
            index: FrameIndex::from_raw(index),
            payload: FramePayload::empty(),
            plan,
            surface: (64, 64),
            published_at: Instant::now(),
        }
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

    // ── §4.2：最新帧槽 ───────────────────────────────────────────────────────

    #[test]
    fn the_slot_is_empty_until_a_frame_is_published() {
        let mut slot = LatestFrame::new();
        assert!(!slot.has_pending());
        assert!(slot.take().is_none());

        slot.publish(request(1, Plan::Full));
        assert!(slot.has_pending());
        let frame = slot.take().expect("published frame");
        assert_eq!(frame.index.get(), 1);
        assert_eq!(frame.surface, (64, 64));
        assert!(!slot.has_pending(), "取走后槽位必须为空");
    }

    #[test]
    fn a_newer_frame_replaces_the_pending_one() {
        let mut slot = LatestFrame::new();
        slot.publish(request(1, partial(&[PhysicalRect::new(0, 0, 8, 8)])));
        slot.publish(request(2, partial(&[PhysicalRect::new(8, 0, 16, 8)])));

        let frame = slot.take().expect("latest frame");
        assert_eq!(frame.index.get(), 2, "旧序号被丢弃（它不产生 GPU 工作）");
        assert!(!slot.has_pending());
    }

    #[test]
    fn replacing_a_frame_merges_the_dropped_damage() {
        let mut slot = LatestFrame::new();
        let first = PhysicalRect::new(0, 0, 8, 8);
        let second = PhysicalRect::new(0, 8, 8, 16);
        slot.publish(request(1, partial(&[first])));
        slot.publish(request(2, partial(&[second])));

        match slot.take().expect("latest frame").plan {
            Plan::Partial(rects) => {
                assert_eq!(rects.as_slice(), &[first, second], "两帧的 damage 取并集");
            }
            other => panic!("unexpected plan: {other:?}"),
        }
    }

    #[test]
    fn replacing_a_full_frame_stays_full() {
        let mut slot = LatestFrame::new();
        slot.publish(request(1, Plan::Full));
        slot.publish(request(2, partial(&[PhysicalRect::new(0, 0, 4, 4)])));
        assert_eq!(slot.take().expect("latest frame").plan, Plan::Full);
    }

    #[test]
    fn replacing_a_partial_frame_by_a_full_frame_is_full() {
        let mut slot = LatestFrame::new();
        slot.publish(request(1, partial(&[PhysicalRect::new(0, 0, 4, 4)])));
        slot.publish(request(2, Plan::Full));
        assert_eq!(slot.take().expect("latest frame").plan, Plan::Full);
    }
}
