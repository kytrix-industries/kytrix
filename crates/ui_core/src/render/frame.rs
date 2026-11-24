use std::{
    mem::size_of,
    ops::Range,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use crate::math::LogicalSize;
use crate::{
    paint::instance::{FrameOp, GeometryDrawGpu, GpuClipEntry, GpuPrimitive, InstanceBuffer},
    render::{compositor::Plan, stats::RenderStats, timer::GpuTimerSample},
    window::FrameIndex,
};

const INSTANCE_BYTES: usize = size_of::<GpuPrimitive>();

pub(crate) struct InstanceChunk {
    start: u32,
    bytes: Arc<[u8]>,
}

impl InstanceChunk {
    pub(crate) fn range(&self) -> Range<u32> {
        self.start..self.start + (self.bytes.len() / INSTANCE_BYTES) as u32
    }

    #[cfg(all(test, windows))]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub(crate) struct FramePayload {
    chunks: Vec<InstanceChunk>,
    instance_count: u32,
    drawn_instances: u32,
    ops: Vec<FrameOp>,
    clips: Vec<GpuClipEntry>,
    geometry: Vec<GeometryDrawGpu>,
    scroll: Vec<[f32; 2]>,
    scroll_clip: Vec<[i32; 4]>,
}

impl FramePayload {
    pub(crate) fn from_buffer_delta(
        buffer: &mut InstanceBuffer,
        scroll: Vec<[f32; 2]>,
        scroll_clip: Vec<[i32; 4]>,
    ) -> Self {
        let mut buffers = pool_lock().pop().unwrap_or_default();
        buffers.chunks.clear();
        let count = buffer.instance_count();
        let ranges = buffer.changes().len();
        for index in 0..ranges {
            let range = buffer.changes()[index].clone();
            let bytes = &buffer.as_bytes()
                [range.start as usize * INSTANCE_BYTES..range.end as usize * INSTANCE_BYTES];
            buffers.chunks.push(InstanceChunk {
                start: range.start,
                bytes: Arc::from(bytes),
            });
        }
        buffer.clear_changes();
        buffers.ops.clear();
        buffers.ops.extend_from_slice(buffer.ops());
        buffers.clips.clear();
        buffer.write_clips_gpu(&mut buffers.clips);
        buffers.geometry.clear();
        buffers.geometry.extend_from_slice(buffer.geometry());
        buffers.scroll = scroll;
        buffers.scroll_clip = scroll_clip;
        Self {
            chunks: buffers.chunks,
            instance_count: count,
            drawn_instances: buffer.len() as u32,
            ops: buffers.ops,
            clips: buffers.clips,
            geometry: buffers.geometry,
            scroll: buffers.scroll,
            scroll_clip: buffers.scroll_clip,
        }
    }

    pub(crate) fn full(
        buffer: &InstanceBuffer,
        scroll: Vec<[f32; 2]>,
        scroll_clip: Vec<[i32; 4]>,
    ) -> Self {
        let mut buffers = pool_lock().pop().unwrap_or_default();
        buffers.chunks.clear();
        let count = buffer.instance_count();
        if count > 0 {
            buffers.chunks.push(InstanceChunk {
                start: 0,
                bytes: Arc::from(buffer.as_bytes()),
            });
        }
        buffers.ops.clear();
        buffers.ops.extend_from_slice(buffer.ops());
        buffers.clips.clear();
        buffer.write_clips_gpu(&mut buffers.clips);
        buffers.geometry.clear();
        buffers.geometry.extend_from_slice(buffer.geometry());
        buffers.scroll = scroll;
        buffers.scroll_clip = scroll_clip;
        Self {
            chunks: buffers.chunks,
            instance_count: count,
            drawn_instances: buffer.len() as u32,
            ops: buffers.ops,
            clips: buffers.clips,
            geometry: buffers.geometry,
            scroll: buffers.scroll,
            scroll_clip: buffers.scroll_clip,
        }
    }

    pub(crate) fn absorb(&mut self, mut dropped: Self) {
        let dropped_chunks = std::mem::take(&mut dropped.chunks);
        if dropped_chunks.is_empty() {
            return;
        }
        let mut kept: Vec<InstanceChunk> = Vec::with_capacity(dropped_chunks.len());
        let exhaustive = dropped_chunks.len().saturating_mul(self.chunks.len()) <= 4096;
        for older in dropped_chunks {
            let range = older.range();
            if range.end > self.instance_count {
                continue;
            }
            let covered = exhaustive
                && self.chunks.iter().any(|newer| {
                    newer.range().start <= range.start && range.end <= newer.range().end
                });
            if !covered {
                kept.push(older);
            }
        }
        kept.append(&mut self.chunks);
        self.chunks = kept;
    }

    pub(crate) fn chunks(&self) -> &[InstanceChunk] {
        &self.chunks
    }

    pub(crate) fn instance_count(&self) -> u32 {
        self.instance_count
    }

    pub(crate) fn drawn_instances(&self) -> u32 {
        self.drawn_instances
    }

    pub(crate) fn ops(&self) -> &[FrameOp] {
        &self.ops
    }

    pub(crate) fn clips(&self) -> &[GpuClipEntry] {
        &self.clips
    }

    pub(crate) fn geometry(&self) -> &[GeometryDrawGpu] {
        &self.geometry
    }

    pub(crate) fn scroll(&self) -> &[[f32; 2]] {
        &self.scroll
    }

    pub(crate) fn scroll_clip(&self) -> &[[i32; 4]] {
        &self.scroll_clip
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn uploaded_bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|chunk| chunk.bytes.len())
            .sum::<usize>()
    }

    #[cfg(any(windows, test))]
    pub(crate) fn empty() -> Self {
        Self {
            chunks: Vec::new(),
            instance_count: 0,
            drawn_instances: 0,
            ops: Vec::new(),
            clips: Vec::new(),
            geometry: Vec::new(),
            scroll: Vec::new(),
            scroll_clip: Vec::new(),
        }
    }
}

#[derive(Default)]
struct PayloadBuffers {
    chunks: Vec<InstanceChunk>,
    ops: Vec<FrameOp>,
    clips: Vec<GpuClipEntry>,
    geometry: Vec<GeometryDrawGpu>,
    scroll: Vec<[f32; 2]>,
    scroll_clip: Vec<[i32; 4]>,
}

const POOL_LIMIT: usize = 4;

fn payload_pool() -> &'static Mutex<Vec<PayloadBuffers>> {
    static POOL: OnceLock<Mutex<Vec<PayloadBuffers>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(Vec::new()))
}

fn pool_lock() -> std::sync::MutexGuard<'static, Vec<PayloadBuffers>> {
    payload_pool()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

impl Drop for FramePayload {
    fn drop(&mut self) {
        if self.chunks.capacity() == 0
            && self.ops.capacity() == 0
            && self.clips.capacity() == 0
            && self.geometry.capacity() == 0
            && self.scroll.capacity() == 0
            && self.scroll_clip.capacity() == 0
        {
            return;
        }
        let mut pool = pool_lock();
        if pool.len() >= POOL_LIMIT {
            return;
        }
        pool.push(PayloadBuffers {
            chunks: std::mem::take(&mut self.chunks),
            ops: std::mem::take(&mut self.ops),
            clips: std::mem::take(&mut self.clips),
            geometry: std::mem::take(&mut self.geometry),
            scroll: std::mem::take(&mut self.scroll),
            scroll_clip: std::mem::take(&mut self.scroll_clip),
        });
    }
}

pub(crate) struct FrameRequest {
    pub index: FrameIndex,
    pub payload: FramePayload,
    pub plan: Plan,
    pub surface: (u32, u32),
    pub scale: f32,
    #[cfg(target_os = "linux")]
    pub surface_local: LogicalSize,
    #[cfg(target_os = "linux")]
    pub buffer_scale: i32,
    pub published_at: Instant,
}

impl FrameRequest {
    pub(crate) fn from_header(
        header: crate::render::delta::FrameHeader,
        payload: FramePayload,
    ) -> Self {
        Self {
            index: header.index,
            payload,
            plan: header.plan,
            surface: header.surface,
            scale: header.scale.factor(),
            #[cfg(target_os = "linux")]
            surface_local: header.surface_local,
            #[cfg(target_os = "linux")]
            buffer_scale: header.buffer_scale,
            published_at: header.published_at,
        }
    }
}

pub(crate) trait FrameSink: Send + Sync {
    fn submit(&self, request: FrameRequest) -> bool;

    fn is_alive(&self) -> bool;

    fn reset_epoch(&self) -> u64 {
        0
    }
}

pub(crate) struct FrameFeedback {
    pub index: FrameIndex,
    pub stats: RenderStats,
    pub samples: Vec<GpuTimerSample>,
    pub completed_at: Instant,
    pub presented_at: Option<Instant>,
    pub latency: Duration,
}

impl FrameFeedback {
    pub(crate) fn presentation_is_consistent(&self) -> bool {
        self.presented_at
            .is_none_or(|presented| presented >= self.completed_at)
    }
}

pub(crate) struct LatestFrame {
    pending: Option<FrameRequest>,
}

impl LatestFrame {
    pub(crate) const fn new() -> Self {
        Self { pending: None }
    }

    pub(crate) fn publish(&mut self, request: FrameRequest) {
        let Some(previous) = self.pending.take() else {
            self.pending = Some(request);
            return;
        };
        let mut next = request;
        next.plan = previous.plan.merge(next.plan);
        next.payload.absorb(previous.payload);
        self.pending = Some(next);
    }

    pub(crate) fn take(&mut self) -> Option<FrameRequest> {
        self.pending.take()
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[derive(Debug, Default)]
pub(crate) struct ResyncGate {
    seen: Option<u64>,
}

impl ResyncGate {
    pub(crate) fn observe(&mut self, epoch: u64) -> bool {
        match self.seen {
            None => {
                self.seen = Some(epoch);
                false
            }
            Some(seen) if seen == epoch => false,
            Some(_) => {
                self.seen = Some(epoch);
                true
            }
        }
    }
}

const MAX_PENDING_RANGES: usize = 128;

pub(crate) struct InstanceMirror {
    bytes: Vec<u8>,
    slots: Vec<SlotMirror>,
    writes: Vec<Range<u32>>,
    write_full: bool,
    payload_ranges: Vec<Range<u32>>,
}

struct SlotMirror {
    needs_full: bool,
    pending: Vec<Range<u32>>,
}

impl InstanceMirror {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            bytes: Vec::new(),
            slots: (0..slots)
                .map(|_| SlotMirror {
                    needs_full: true,
                    pending: Vec::new(),
                })
                .collect(),
            writes: Vec::new(),
            write_full: false,
            payload_ranges: Vec::new(),
        }
    }

    pub(crate) fn invalidate(&mut self, slot: usize) {
        let state = &mut self.slots[slot];
        state.needs_full = true;
        state.pending.clear();
    }

    pub(crate) fn prepare(&mut self, slot: usize, payload: &FramePayload) -> bool {
        let Self {
            bytes,
            slots,
            writes,
            write_full,
            payload_ranges,
        } = self;
        let count = payload.instance_count();
        let needed = count as usize * INSTANCE_BYTES;
        if bytes.len() < needed {
            bytes.resize(needed, 0);
        } else {
            bytes.truncate(needed);
        }
        payload_ranges.clear();
        for chunk in payload.chunks() {
            let range = chunk.range();
            let start = range.start as usize * INSTANCE_BYTES;
            let end = start + chunk.bytes.len();
            assert!(
                end <= bytes.len(),
                "payload chunk escapes the instance arena"
            );
            bytes[start..end].copy_from_slice(&chunk.bytes);
            payload_ranges.push(range);
        }
        normalize_ranges(payload_ranges);
        for state in slots.iter_mut() {
            state
                .pending
                .retain(|range| range.start < count && range.end <= count);
        }
        writes.clear();
        *write_full = false;
        let state = &mut slots[slot];
        let outstanding: u32 = state
            .pending
            .iter()
            .map(|range| range.end - range.start)
            .sum();
        if state.needs_full || outstanding.saturating_mul(2) >= count {
            *write_full = true;
            return true;
        }
        writes.extend(state.pending.iter().cloned());
        writes.extend(payload_ranges.iter().cloned());
        normalize_ranges(writes);
        let written: u32 = writes.iter().map(|range| range.end - range.start).sum();
        if written.saturating_mul(2) >= count {
            *write_full = true;
        }
        *write_full
    }

    pub(crate) fn writes(&self) -> &[Range<u32>] {
        &self.writes
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn finish(&mut self, slot: usize) {
        let Self {
            slots,
            write_full,
            payload_ranges,
            writes,
            ..
        } = self;
        for (index, state) in slots.iter_mut().enumerate() {
            if index == slot {
                state.needs_full = false;
                state.pending.clear();
                continue;
            }
            for range in payload_ranges.iter() {
                push_range(&mut state.pending, range.clone());
            }
            if state.pending.len() > MAX_PENDING_RANGES {
                state.needs_full = true;
                state.pending.clear();
            }
        }
        writes.clear();
        *write_full = false;
    }
}

fn push_range(ranges: &mut Vec<Range<u32>>, range: Range<u32>) {
    if range.is_empty() {
        return;
    }
    let mut start = range.start;
    let mut end = range.end;
    let mut index = 0usize;
    while index < ranges.len() {
        let current = &ranges[index];
        if current.end < start || current.start > end {
            index += 1;
            continue;
        }
        start = start.min(current.start);
        end = end.max(current.end);
        ranges.remove(index);
    }
    ranges.push(start..end);
}

fn normalize_ranges(ranges: &mut Vec<Range<u32>>) {
    ranges.sort_unstable_by_key(|range| range.start);
    let mut out = 0usize;
    for read in 0..ranges.len() {
        let range = ranges[read].clone();
        if out > 0 {
            let last = &mut ranges[out - 1];
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        ranges[out] = range;
        out += 1;
    }
    ranges.truncate(out);
}

pub(crate) const MAX_FRAMES_IN_FLIGHT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameSlot {
    pub index: usize,
    pub wait_value: u64,
    pub signal_value: u64,
}

pub(crate) struct InFlightRing {
    slot_values: [u64; MAX_FRAMES_IN_FLIGHT],
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

    pub(crate) fn acquire(&mut self) -> FrameSlot {
        let index = self.next_slot;
        self.next_slot = (index + 1) % MAX_FRAMES_IN_FLIGHT;
        FrameSlot {
            index,
            wait_value: self.slot_values[index],
            signal_value: self.next_value + 1,
        }
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn peek(&self) -> FrameSlot {
        let index = self.next_slot;
        FrameSlot {
            index,
            wait_value: self.slot_values[index],
            signal_value: self.next_value + 1,
        }
    }

    pub(crate) fn advance(&mut self, slot: FrameSlot) {
        self.slot_values[slot.index] = slot.signal_value;
        self.next_value = slot.signal_value;
    }

    pub(crate) fn last_signaled(&self) -> u64 {
        self.next_value
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use super::{
        FrameFeedback, FramePayload, FrameRequest, FrameSlot, InFlightRing, InstanceChunk,
        InstanceMirror, LatestFrame, MAX_FRAMES_IN_FLIGHT, ResyncGate, pool_lock,
    };
    #[cfg(target_os = "linux")]
    use crate::math::px;
    use crate::{
        math::{DeviceRect, dp},
        paint::instance::InstanceEncoder,
        render::{
            compositor::{DamageRects, Plan},
            stats::RenderStats,
        },
        window::FrameIndex,
    };

    #[test]
    fn a_feedback_never_claims_a_presented_time_before_the_gpu_completed() {
        let completed_at = Instant::now();
        let feedback = |presented_at: Option<Instant>| FrameFeedback {
            index: FrameIndex::from_raw(1),
            stats: RenderStats::default(),
            samples: Vec::new(),
            completed_at,
            presented_at,
            latency: Duration::from_millis(4),
        };
        assert!(
            feedback(None).presentation_is_consistent(),
            "该端不提供上屏时刻时 `None` 是合法值"
        );
        assert!(feedback(Some(completed_at)).presentation_is_consistent());
        assert!(
            feedback(Some(completed_at + Duration::from_millis(8))).presentation_is_consistent(),
            "上屏必然晚于 GPU 完成：tick 推断值也遵守"
        );
        assert!(
            !feedback(Some(completed_at - Duration::from_millis(1))).presentation_is_consistent(),
            "早于 GPU 完成的上屏时刻不可信"
        );
    }

    #[test]
    fn a_dropped_payload_returns_its_buffers_to_the_pool() {
        let mut pool = pool_lock();
        let before = pool.len();
        let mut seeded = super::PayloadBuffers::default();
        seeded.chunks.reserve(4096);
        pool.push(seeded);
        assert_eq!(pool.len(), before + 1, "入池必须成功");
        let taken = pool.pop().expect("刚种下的缓冲必须能取回");
        assert!(
            taken.chunks.capacity() >= 4096,
            "池必须保留容量（capacity={}）",
            taken.chunks.capacity()
        );
        drop(taken);
        drop(pool);

        let encoder = InstanceEncoder::new();
        let payload = FramePayload::full(encoder.buffer(), Vec::new(), Vec::new());
        assert_eq!(payload.instance_count(), 0);
        drop(payload);
    }

    fn partial(rects: &[DeviceRect]) -> Plan {
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
            surface_local: crate::math::LogicalSize::new(px(64.0), px(64.0)),
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            published_at: Instant::now(),
        }
    }

    #[test]
    fn peek_matches_the_next_acquire() {
        let mut ring = InFlightRing::new();
        for expected in [0, 1, 2, 0] {
            let peeked = ring.peek();
            assert_eq!(peeked.index, expected, "peek 不得推进轮转");
            let acquired = ring.acquire();
            assert_eq!(acquired.index, peeked.index, "peek 必须先于 acquire 一致");
            assert_eq!(
                acquired.signal_value, peeked.signal_value,
                "peek 给出的 fence 值也必须与 acquire 一致"
            );
            ring.advance(acquired);
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
    fn timeline_values_track_submissions_not_ui_frame_indices() {
        let mut ring = InFlightRing::new();
        let mut signaled = Vec::new();
        for ui_index in [1u32, 4, 7] {
            let slot = ring.acquire();
            signaled.push((ui_index, slot.signal_value));
            ring.advance(slot);
        }
        assert_eq!(
            signaled,
            vec![(1, 1), (4, 2), (7, 3)],
            "UI 帧序号与提交序号是两条独立序列"
        );
        assert_eq!(
            ring.last_signaled(),
            3,
            "teardown 等的是最后一次 signal 的值（3），不是最后的 UI 帧序号（7）"
        );
        assert!(
            signaled
                .iter()
                .all(|(_, value)| *value <= ring.last_signaled())
        );
    }

    #[test]
    fn retries_never_signal_the_same_timeline_value_twice() {
        let mut ring = InFlightRing::new();
        let retried = ring.acquire();
        let submitted = ring.acquire();
        assert_eq!(
            submitted.signal_value, retried.signal_value,
            "未提交的槽不消费提交序"
        );
        ring.advance(submitted);
        let next = ring.acquire();
        assert!(next.signal_value > submitted.signal_value);
        ring.advance(next);
        assert_eq!(ring.last_signaled(), next.signal_value);
    }

    #[test]
    fn advance_only_touches_its_own_slot() {
        let mut ring = InFlightRing::new();
        let first = ring.acquire();
        ring.advance(first);
        let second = ring.acquire();
        ring.advance(FrameSlot {
            index: second.index,
            wait_value: 0,
            signal_value: 41,
        });
        let third = ring.acquire();
        assert_eq!(third.index, 2);
        assert_eq!(third.wait_value, 0);
        ring.advance(third);
        let fourth = ring.acquire();
        assert_eq!(fourth.index, 0);
        assert_eq!(fourth.wait_value, first.signal_value);
    }

    #[test]
    fn acquire_rotates_slots_by_acquisition_count() {
        let mut frame = LatestFrame::new();
        let mut ring = InFlightRing::new();
        let mut submitted = Vec::new();
        for value in 1..=10 {
            frame.publish(request(value, Plan::Full));
            if value % 3 != 1 {
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
        slot.publish(request(
            1,
            partial(&[DeviceRect::new(dp(0), dp(0), dp(8), dp(8))]),
        ));
        slot.publish(request(
            2,
            partial(&[DeviceRect::new(dp(8), dp(0), dp(16), dp(8))]),
        ));

        let frame = slot.take().expect("latest frame");
        assert_eq!(frame.index.get(), 2, "旧序号被丢弃（它不产生 GPU 工作）");
        assert!(!slot.has_pending());
    }

    #[test]
    fn replacing_a_frame_merges_the_dropped_damage() {
        let mut slot = LatestFrame::new();
        let first = DeviceRect::new(dp(0), dp(0), dp(8), dp(8));
        let second = DeviceRect::new(dp(0), dp(8), dp(8), dp(16));
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
        slot.publish(request(
            2,
            partial(&[DeviceRect::new(dp(0), dp(0), dp(4), dp(4))]),
        ));
        assert_eq!(slot.take().expect("latest frame").plan, Plan::Full);
    }

    #[test]
    fn replacing_a_partial_frame_by_a_full_frame_is_full() {
        let mut slot = LatestFrame::new();
        slot.publish(request(
            1,
            partial(&[DeviceRect::new(dp(0), dp(0), dp(4), dp(4))]),
        ));
        slot.publish(request(2, Plan::Full));
        assert_eq!(slot.take().expect("latest frame").plan, Plan::Full);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn replacing_a_frame_keeps_surface_and_surface_local_paired() {
        use crate::math::LogicalSize;

        let mut slot = LatestFrame::new();
        slot.publish(FrameRequest {
            surface: (800, 600),
            scale: 1.0,
            surface_local: LogicalSize::new(px(800.0), px(600.0)),
            buffer_scale: 1,
            ..request(1, Plan::Full)
        });
        slot.publish(FrameRequest {
            surface: (876, 626),
            scale: 1.0,
            surface_local: LogicalSize::new(px(701.0), px(501.0)),
            buffer_scale: 2,
            ..request(2, Plan::Full)
        });

        let frame = slot.take().expect("latest frame");
        assert_eq!(frame.surface, (876, 626), "swapchain 目标来自新帧");
        assert_eq!(
            frame.surface_local,
            LogicalSize::new(px(701.0), px(501.0)),
            "destination 与 buffer 必须同帧成对"
        );
        assert_eq!(frame.buffer_scale, 2, "buffer scale 与 buffer 必须同帧成对");
    }

    fn manual_payload(chunks: &[(u32, &[u8])], count: u32) -> FramePayload {
        FramePayload {
            chunks: chunks
                .iter()
                .map(|(start, bytes)| InstanceChunk {
                    start: *start,
                    bytes: Arc::from(*bytes),
                })
                .collect(),
            instance_count: count,
            drawn_instances: count,
            ops: Vec::new(),
            clips: Vec::new(),
            geometry: Vec::new(),
            scroll: Vec::new(),
            scroll_clip: Vec::new(),
        }
    }

    fn apply_mirror(mirror: &InstanceMirror, full: bool, target: &mut [u8]) {
        if full {
            target.copy_from_slice(&mirror.bytes()[..target.len()]);
            return;
        }
        for range in mirror.writes() {
            let start = range.start as usize * 128;
            let end = range.end as usize * 128;
            target[start..end].copy_from_slice(&mirror.bytes()[start..end]);
        }
    }

    #[test]
    fn a_replaced_frame_absorbs_its_instance_chunks() {
        let mut newer = manual_payload(&[(0, &[9u8; 128])], 8);
        let older = manual_payload(&[(2, &[7u8; 256])], 8);
        newer.absorb(older);
        let starts: Vec<u32> = newer.chunks.iter().map(|chunk| chunk.start).collect();
        assert_eq!(
            starts,
            vec![2, 0],
            "应用顺序必须是「旧块在前、新块在后」（重叠时新块覆盖）"
        );
    }

    #[test]
    fn absorb_drops_chunks_fully_covered_by_newer_bytes() {
        let mut newer = manual_payload(&[(0, &[2u8; 512])], 8);
        let older = manual_payload(&[(1, &[1u8; 256])], 8);
        newer.absorb(older);
        assert_eq!(
            newer.chunks.len(),
            1,
            "被覆盖的旧块必须丢掉：{:?}",
            newer
                .chunks
                .iter()
                .map(InstanceChunk::range)
                .collect::<Vec<_>>()
        );
        assert_eq!(newer.chunks[0].start, 0);
    }

    #[test]
    fn a_dropped_frame_never_contributes_bytes_outside_a_smaller_arena() {
        let older = manual_payload(&[(0, &[7u8; 12 * 128])], 12);
        let mut newer = manual_payload(&[(0, &[9u8; 5 * 128])], 5);
        newer.absorb(older);
        let ranges: Vec<std::ops::Range<u32>> =
            newer.chunks().iter().map(InstanceChunk::range).collect();
        assert!(
            ranges
                .iter()
                .all(|range| range.end <= newer.instance_count()),
            "合并后的块必须落在实例环内（块 {ranges:?}，环 0..{}）",
            newer.instance_count()
        );
        let mut mirror = InstanceMirror::new(1);
        assert!(mirror.prepare(0, &newer), "合并后的载荷必须整块写进镜像");
        assert!(
            mirror.bytes().iter().all(|byte| *byte == 9),
            "镜像里只允许出现新帧的字节"
        );
    }

    #[test]
    fn replacing_a_pending_frame_merges_its_instance_chunks() {
        let mut slot = LatestFrame::new();
        let mut first = request(1, Plan::Full);
        first.payload = manual_payload(&[(4, &[1u8; 128])], 8);
        slot.publish(first);
        let mut second = request(2, partial(&[DeviceRect::new(dp(0), dp(0), dp(8), dp(8))]));
        second.payload = manual_payload(&[(0, &[2u8; 128])], 8);
        slot.publish(second);

        let frame = slot.take().expect("latest frame");
        assert_eq!(frame.index.get(), 2, "新序号胜出");
        let starts: Vec<u32> = frame.payload.chunks().iter().map(|c| c.start).collect();
        assert_eq!(starts, vec![4, 0], "旧帧的实例块必须并进新帧");
    }

    #[test]
    fn the_instance_mirror_converges_every_slot_after_rotation() {
        let mut mirror = InstanceMirror::new(3);
        let first = manual_payload(&[(0, &[1u8; 16 * 128])], 16);
        assert!(mirror.prepare(0, &first), "从未写过的槽必须整块写");
        let mut gpu = [
            vec![0u8; 16 * 128],
            vec![0u8; 16 * 128],
            vec![0u8; 16 * 128],
        ];
        apply_mirror(&mirror, true, &mut gpu[0]);
        mirror.finish(0);
        assert_eq!(gpu[0], mirror.bytes(), "槽 0 与镜像一致");

        let second = manual_payload(&[(3, &[2u8; 128])], 16);
        assert!(mirror.prepare(1, &second));
        apply_mirror(&mirror, true, &mut gpu[1]);
        mirror.finish(1);
        assert_eq!(gpu[1], mirror.bytes());

        let third = manual_payload(&[(0, &[3u8; 128])], 16);
        assert!(!mirror.prepare(0, &third), "欠账 + 本帧变化不足以整块写");
        let writes: Vec<std::ops::Range<u32>> = mirror.writes().to_vec();
        assert_eq!(writes, vec![0..1, 3..4], "只写「欠账 ∪ 本帧变化」的区间");
        apply_mirror(&mirror, false, &mut gpu[0]);
        mirror.finish(0);
        assert_eq!(gpu[0], mirror.bytes(), "增量补写之后槽 0 与镜像一致");

        let fourth = manual_payload(&[(8, &[4u8; 128])], 16);
        assert!(mirror.prepare(2, &fourth));
        apply_mirror(&mirror, true, &mut gpu[2]);
        mirror.finish(2);
        assert_eq!(gpu[2], mirror.bytes());
        assert_eq!(mirror.bytes()[0], 3);
        assert_eq!(mirror.bytes()[128 * 3], 2);
        assert_eq!(mirror.bytes()[128 * 8], 4);
        assert_eq!(mirror.bytes()[128 * 5], 1);
    }

    #[test]
    fn an_invalidated_slot_is_rewritten_in_full() {
        let mut mirror = InstanceMirror::new(1);
        let first = manual_payload(&[(0, &[5u8; 4 * 128])], 4);
        assert!(mirror.prepare(0, &first));
        mirror.finish(0);
        let idle = manual_payload(&[], 4);
        assert!(!mirror.prepare(0, &idle));
        mirror.finish(0);
        mirror.invalidate(0);
        assert!(mirror.prepare(0, &idle), "重构过的槽必须整块重写");
        mirror.finish(0);
    }

    #[test]
    fn the_resync_gate_reports_a_rebuild_only_once() {
        let mut gate = ResyncGate::default();
        assert!(!gate.observe(0), "首次观察只记录代际（首帧本来就全量）");
        assert!(!gate.observe(0), "代际没变不得要求整帧重发");

        assert!(gate.observe(1), "代际变化必须报告一次「本帧整帧发布」");
        assert!(!gate.observe(1), "同一个新代际只报告一次（下一帧回到增量）");

        assert!(gate.observe(2));
        assert!(!gate.observe(2));
        assert!(gate.observe(1));
        assert!(!gate.observe(1));
        let mut stateless = ResyncGate::default();
        assert!(!stateless.observe(0));
        assert!(!stateless.observe(0));
    }
}
