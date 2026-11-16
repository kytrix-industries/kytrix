//! 帧交付契约（共享，纯数据）：`FramePayload` / `FrameRequest` / `FrameFeedback` 与最新帧槽；
//! 以及 in-flight 帧轮转（`design/phase-08-d3d12.md` §4.2）。
//!
//! 本模块是**三平台共用**的纯数据 + 纯函数（`design/async-render-pipeline.md` §4.1 / §4.2）：
//! 不含线程、句柄或任何平台 API。UI 线程产帧、GPU 线程执行，跨线程只搬这里的值。
//! 线程原语（Windows 的 work event、Linux 的 condvar、macOS 的 display link）各自留在平台模块内。
//!
//! `FrameIndex` 本身留在 `window.rs`（P06 的公开 API 返回类型，DEVIATIONS F-74）；本模块只提供
//! 「槽位 + 需要等待/将要 signal 的 fence 值」（`render/windows/queue.rs` 消费）与帧交付数据。

use std::{
    sync::{Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use crate::math::Size;
use crate::{
    paint::instance::{FrameOp, GeometryDrawGpu, GpuClipEntry, InstanceBuffer},
    render::{compositor::Plan, stats::RenderStats, timer::GpuTimerSample},
    window::FrameIndex,
};

/// 一帧的 CPU→GPU 快照（`design/async-render-pipeline.md` §4.1）。
///
/// 实例字节、绘制序、裁剪表、几何表必须一起过线：GPU 线程拿到的必须是**同一帧**的四份数据。
/// 每帧一次拷贝是线程边界的所有权转移，不是可选优化（§4.2 的代价说明）。
pub(crate) struct FramePayload {
    instances: Vec<u8>,
    ops: Vec<FrameOp>,
    clips: Vec<GpuClipEntry>,
    /// 几何绘制表：`FrameOp::Geometry.index` 的下标表，**必须与 `ops` 同帧过线**
    /// （`ops` 里的下标只有配合同一帧的表才有意义）。
    ///
    /// 目前只有 Windows 的录制路径消费它（macOS / Linux 的 `FrameOp::Geometry` 还没有 GPU
    /// 缓冲来源），但三平台的 payload 形状保持一致：它是「一帧的全部 CPU 侧数据」，不是按
    /// 平台裁剪的数据包。
    geometry: Vec<GeometryDrawGpu>,
}

/// 一帧的四个 CPU 缓冲（`FramePayload` 回收的单位，DEVIATIONS F-213）。
#[derive(Default)]
struct PayloadBuffers {
    instances: Vec<u8>,
    ops: Vec<FrameOp>,
    clips: Vec<GpuClipEntry>,
    geometry: Vec<GeometryDrawGpu>,
}

/// 回收池上限：最多缓存 4 帧的缓冲（GPU 线程通常只压着一帧，4 个足够且封顶内存）。
const POOL_LIMIT: usize = 4;

/// 跨线程回收池：GPU 线程 drop `FramePayload` 时把缓冲还回来，UI 线程下一帧直接复用。
///
/// `Mutex` 每帧只锁一次、临界区是 4 次 `Vec` 交换，代价远小于 4 次分配。
fn payload_pool() -> &'static Mutex<Vec<PayloadBuffers>> {
    static POOL: OnceLock<Mutex<Vec<PayloadBuffers>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Vec::new()))
}

fn pool_lock() -> std::sync::MutexGuard<'static, Vec<PayloadBuffers>> {
    payload_pool()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

impl FramePayload {
    /// 从 UI 线程的编码结果复制一份（缓冲从回收池取，容量跨帧复用）。
    pub(crate) fn from_buffer(buffer: &InstanceBuffer) -> Self {
        let mut buffers = pool_lock().pop().unwrap_or_default();
        buffers.instances.clear();
        buffers.instances.extend_from_slice(buffer.as_bytes());
        buffers.ops.clear();
        buffers.ops.extend_from_slice(buffer.ops());
        buffers.clips.clear();
        buffer.write_clips_gpu(&mut buffers.clips);
        buffers.geometry.clear();
        buffers.geometry.extend_from_slice(buffer.geometry());
        Self {
            instances: buffers.instances,
            ops: buffers.ops,
            clips: buffers.clips,
            geometry: buffers.geometry,
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
    pub(crate) fn geometry(&self) -> &[GeometryDrawGpu] {
        &self.geometry
    }

    /// 空载荷（窗口首帧 priming 与槽位/线程单元测试用；真实帧必须走
    /// [`FramePayload::from_buffer`]）。只有 Windows 的首帧 priming 在产品构建里需要它。
    #[cfg(any(windows, test))]
    pub(crate) fn empty() -> Self {
        Self {
            instances: Vec::new(),
            ops: Vec::new(),
            clips: Vec::new(),
            geometry: Vec::new(),
        }
    }
}

impl Drop for FramePayload {
    fn drop(&mut self) {
        // 只回收有容量的缓冲：`empty()` 的零容量帧不占池位。
        if self.instances.capacity() == 0
            && self.ops.capacity() == 0
            && self.clips.capacity() == 0
            && self.geometry.capacity() == 0
        {
            return;
        }
        let mut pool = pool_lock();
        if pool.len() >= POOL_LIMIT {
            return;
        }
        pool.push(PayloadBuffers {
            instances: std::mem::take(&mut self.instances),
            ops: std::mem::take(&mut self.ops),
            clips: std::mem::take(&mut self.clips),
            geometry: std::mem::take(&mut self.geometry),
        });
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
    /// 窗口 DPI（`root.frame.params.x`）。
    ///
    /// **必须与 `surface` 同帧过线**：geometry 通道的顶点是逻辑像素，shader 用 `frame.params.x`
    /// 换物理像素；两者出自不同帧时，DPI 切换后的第一帧几何会被画在错误的尺度上
    /// （实例通道不受影响，它的 bounds 已是物理像素 —— 这正是这个 bug 容易被漏掉的原因）。
    pub scale: f32,
    /// 窗口的逻辑尺寸（surface-local 坐标；Linux 专用）。
    ///
    /// Linux 的 Wayland buffer 尺寸是 `round(surface_local × scale)`，而
    /// `wp_viewport.set_destination` 必须写逻辑尺寸（compositor 按它显示 buffer）。两者必须来自
    /// **同一帧**：GPU 线程按本帧的 `surface` 重建 swapchain、按本帧的 `surface_local` 写
    /// destination，present 之前一次生效（D1 / D2）。
    #[cfg(target_os = "linux")]
    pub surface_local: Size,
    /// 整数回退的 `wl_surface` buffer scale（Linux 专用；fractional 模式为 1）。
    ///
    /// 与 `surface` / `surface_local` 同帧成对：整数 `N` 下 `surface = round(surface_local × N)`，
    /// `buffer_scale` 必须是 `N`，surface 的逻辑尺寸才等于 `surface / N`；否则 compositor 会把
    /// 整面放大 `N×`（V-110）。GPU 线程在 present 前写入，与 attach/commit 同一次生效。
    #[cfg(target_os = "linux")]
    pub buffer_scale: i32,
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
    /// ring 槽位（`0..MAX_FRAMES_IN_FLIGHT`），来自 [`InFlightRing::acquire`] 的轮转。
    pub index: usize,
    /// 进入本帧前必须等待到达的 fence 值（0 = 该槽从未使用，无需等待）。
    pub wait_value: u64,
    /// 本帧提交成功后 signal 的值。
    pub signal_value: u64,
}

/// ring 的 fence 记账 + 槽位轮转（「每个槽最后一次提交的值」+ 下一次提交的槽位）。
pub(crate) struct InFlightRing {
    slot_values: [u64; MAX_FRAMES_IN_FLIGHT],
    /// 下一次 [`InFlightRing::acquire`] 给出的槽位（0 → 1 → 2 → 0）。
    next_slot: usize,
    next_value: u64,
}

impl InFlightRing {
    pub(crate) const fn new() -> Self {
        Self {
            slot_values: [0; MAX_FRAMES_IN_FLIGHT],
            next_slot: 0,
            next_value: 0,
        }
    }

    /// 取本帧槽位：按 **acquire 次数**轮转（0 → 1 → 2 → 0），不推进 fence 记账（提交成功后才
    /// [`InFlightRing::advance`]）。正常路径每次 acquire 恰好对应一次提交；`NOT_READY` /
    /// `OUT_OF_DATE` 重试会多消费一次 acquire 而不提交（复用槽之前由调用方等的槽 fence 保证安全）。
    ///
    /// 槽位**不能**由 `FrameIndex` 取模决定：最新帧槽合并时会丢掉被覆盖帧的序号
    /// （[`LatestFrame`]），序号在一次提交与下一次提交之间的跨度可以是任意值；而平台的背压
    /// 原语（macOS 的三 permit semaphore、三槽上传缓冲）按提交次数轮转。两者不同源时，跨度
    /// 为 3 的倍数的两个提交会落到同一个上传槽 —— 后一个在 CPU 上重写 GPU 仍在读的缓冲
    /// （内容损坏，Metal validation 检测不到）。
    pub(crate) fn acquire(&mut self) -> FrameSlot {
        let index = self.next_slot;
        self.next_slot = (index + 1) % MAX_FRAMES_IN_FLIGHT;
        FrameSlot {
            index,
            wait_value: self.slot_values[index],
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{
        FramePayload, FrameRequest, FrameSlot, InFlightRing, LatestFrame, MAX_FRAMES_IN_FLIGHT,
        pool_lock,
    };
    use crate::{
        math::PhysicalRect,
        paint::instance::InstanceEncoder,
        render::compositor::{DamageRects, Plan},
        window::FrameIndex,
    };

    #[test]
    fn a_dropped_payload_returns_its_buffers_to_the_pool() {
        // 回收池是跨线程的：GPU 线程 drop 帧后，UI 线程下一帧直接复用容量（F-213）。
        let mut seeded = super::PayloadBuffers::default();
        seeded.instances.reserve(4096);
        pool_lock().push(seeded);

        let encoder = InstanceEncoder::new();
        let payload = FramePayload::from_buffer(encoder.buffer());
        let capacity = payload.instances.capacity();
        assert!(capacity >= 4096, "from_buffer 必须复用池里的容量");
        drop(payload);
        let pooled = pool_lock()
            .iter()
            .map(|buffers| buffers.instances.capacity())
            .max();
        assert!(
            pooled.is_some_and(|pooled| pooled >= capacity),
            "缓冲必须回收到池并保留容量（pooled={pooled:?}）"
        );
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
            scale: 1.0,
            #[cfg(target_os = "linux")]
            surface_local: crate::math::Size::new(64.0, 64.0),
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            published_at: Instant::now(),
        }
    }

    #[test]
    fn slots_cycle_through_the_ring() {
        let mut ring = InFlightRing::new();
        let slots: Vec<usize> = (0..7).map(|_| ring.acquire().index).collect();
        assert_eq!(slots, vec![0, 1, 2, 0, 1, 2, 0]);
        assert_eq!(MAX_FRAMES_IN_FLIGHT, 3);
    }

    #[test]
    fn first_use_of_each_slot_has_no_wait() {
        let mut ring = InFlightRing::new();
        for _ in 0..MAX_FRAMES_IN_FLIGHT {
            let slot = ring.acquire();
            assert_eq!(slot.wait_value, 0, "slot {} must not wait", slot.index);
            ring.advance(slot);
        }
    }

    #[test]
    fn wait_value_is_the_previous_submission_of_that_slot() {
        let mut ring = InFlightRing::new();
        let first = ring.acquire();
        ring.advance(first);
        let second = ring.acquire();
        ring.advance(second);
        let third = ring.acquire();
        ring.advance(third);

        // 第 4 次提交回到 slot 0（第 1 次提交的槽），必须等第 1 次的 signal 值。
        let fourth = ring.acquire();
        assert_eq!(fourth.index, 0);
        assert_eq!(fourth.wait_value, first.signal_value);
        assert_eq!(fourth.signal_value, 4);
    }

    #[test]
    fn signal_values_are_monotonic() {
        let mut ring = InFlightRing::new();
        let mut previous = 0;
        for _ in 0..9 {
            let slot = ring.acquire();
            assert_eq!(slot.signal_value, previous + 1);
            previous = slot.signal_value;
            ring.advance(slot);
        }
        assert_eq!(ring.last_signaled(), 9);
    }

    #[test]
    fn advance_only_touches_its_own_slot() {
        let mut ring = InFlightRing::new();
        let first = ring.acquire();
        ring.advance(first);
        let second = ring.acquire();
        // 手动写入一个非连续的值：它只能影响 slot 1 的记账。
        ring.advance(FrameSlot {
            index: second.index,
            wait_value: 0,
            signal_value: 41,
        });
        // slot 2 从未提交过，等待值必须是 0（不能被 slot 1 的 41 污染）。
        let third = ring.acquire();
        assert_eq!(third.index, 2);
        assert_eq!(third.wait_value, 0);
        ring.advance(third);
        // 回到 slot 0：等待值仍是第 1 次提交的值。
        let fourth = ring.acquire();
        assert_eq!(fourth.index, 0);
        assert_eq!(fourth.wait_value, first.signal_value);
    }

    /// P0 回归：**不重试时**槽位按 acquire 次数轮转（0 → 1 → 2），与 `FrameIndex` 无关。
    ///
    /// 这里每 3 个序号才取走一帧（1、4、7、10）：旧实现用 `index % 3` 会一直命中同一个槽，
    /// 而三槽 semaphore / 上传缓冲按提交次数轮转 —— 两者不同源，新帧会重写 GPU 仍在读的
    /// 上传缓冲。本测试只覆盖「每次 acquire 都提交」这一条路径：`NOT_READY` / `OUT_OF_DATE`
    /// 重试也会消费一次 acquire 而不提交（`renderer::run_gpu_loop` 保留 pending 帧），那种
    /// 交错下相邻两次提交不保证落在不同槽（复用槽之前由槽 fence 等待保证安全），不在本测试内。
    #[test]
    fn acquire_rotates_slots_by_acquisition_count() {
        let mut frame = LatestFrame::new();
        let mut ring = InFlightRing::new();
        let mut submitted = Vec::new();
        for value in 1..=10 {
            frame.publish(request(value, Plan::Full));
            if value % 3 != 1 {
                // 被后继帧覆盖：该序号不产生 GPU 工作。
                continue;
            }
            let frame = frame.take().expect("pending frame");
            assert_eq!(frame.index.get(), value, "被覆盖的序号必须被丢弃");
            let slot = ring.acquire();
            let recent = &submitted[submitted.len().saturating_sub(2)..];
            assert!(
                !recent.contains(&slot.index),
                "slot {} reused within three acquisitions (recent: {recent:?})",
                slot.index
            );
            submitted.push(slot.index);
            ring.advance(slot);
        }
        assert_eq!(submitted, vec![0, 1, 2, 0]);
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

    /// Linux：buffer 尺寸、逻辑尺寸与 buffer scale 必须成对来自最新帧。
    ///
    /// destination（`surface_local`）与 swapchain（`surface`）是新帧的两个面，覆盖时保留旧的
    /// 任何一个都会让 compositor 用错尺寸 —— 旧内容被缩放（D1）或整面重采样（D3）。整数回退的
    /// `buffer_scale` 同理：它必须与这一帧的 buffer 尺寸出自同一帧，否则 surface 的逻辑尺寸
    /// 会被乘 / 除错（V-110）。
    #[cfg(target_os = "linux")]
    #[test]
    fn replacing_a_frame_keeps_surface_and_surface_local_paired() {
        use crate::math::Size;

        let mut slot = LatestFrame::new();
        slot.publish(FrameRequest {
            surface: (800, 600),
            scale: 1.0,
            surface_local: Size::new(800.0, 600.0),
            buffer_scale: 1,
            ..request(1, Plan::Full)
        });
        slot.publish(FrameRequest {
            surface: (876, 626),
            scale: 1.0,
            surface_local: Size::new(701.0, 501.0),
            buffer_scale: 2,
            ..request(2, Plan::Full)
        });

        let frame = slot.take().expect("latest frame");
        assert_eq!(frame.surface, (876, 626), "swapchain 目标来自新帧");
        assert_eq!(
            frame.surface_local,
            Size::new(701.0, 501.0),
            "destination 与 buffer 必须同帧成对"
        );
        assert_eq!(frame.buffer_scale, 2, "buffer scale 与 buffer 必须同帧成对");
    }
}
