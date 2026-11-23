#[cfg(any(target_os = "linux", target_os = "macos", test))]
use std::collections::VecDeque;
#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "macos", test))]
use crate::render::{
    RenderStats,
    frame::{FrameFeedback, MAX_FRAMES_IN_FLIGHT},
    timer::GpuTimerSample,
};
#[cfg(any(target_os = "linux", target_os = "macos", test))]
use crate::window::FrameIndex;

#[cfg(any(target_os = "macos", test))]
pub(crate) const IDLE_GRACE: Duration = Duration::from_millis(250);

#[cfg(any(target_os = "macos", test))]
pub(crate) fn pacing_work(
    continuous: bool,
    pending_frame: bool,
    in_flight: usize,
    awaiting_presentation: bool,
) -> bool {
    continuous || pending_frame || in_flight > 0 || awaiting_presentation
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug)]
pub(crate) struct PacingGate {
    last_work: Option<Instant>,
    paused: bool,
    #[cfg(test)]
    pauses: u64,
}

#[cfg(any(target_os = "macos", test))]
impl PacingGate {
    pub(crate) fn new() -> Self {
        Self {
            last_work: None,
            paused: true,
            #[cfg(test)]
            pauses: 0,
        }
    }

    pub(crate) fn note_work(&mut self, now: Instant) {
        self.last_work = Some(now);
        self.paused = false;
    }

    pub(crate) fn update(&mut self, work: bool, now: Instant) -> bool {
        if work {
            self.note_work(now);
            return false;
        }
        let idle = self.last_work.map(|at| now.saturating_duration_since(at));
        let should_pause = idle.is_none_or(|idle| idle >= IDLE_GRACE);
        if should_pause && !self.paused {
            #[cfg(test)]
            {
                self.pauses = self.pauses.saturating_add(1);
            }
        }
        self.paused = should_pause;
        should_pause
    }

    #[cfg(test)]
    const fn pauses(&self) -> u64 {
        self.pauses
    }
}

#[cfg(any(target_os = "macos", test))]
const MAX_TARGET_LEAD: Duration = Duration::from_millis(250);

#[cfg(any(target_os = "macos", test))]
pub(crate) fn presentation_instant(
    target: f64,
    observed: Instant,
    now_host: f64,
) -> Option<Instant> {
    if !target.is_finite() || target <= 0.0 || !now_host.is_finite() {
        return None;
    }
    let lead = target - now_host;
    if !lead.is_finite() || lead.abs() > MAX_TARGET_LEAD.as_secs_f64() {
        return None;
    }
    if lead >= 0.0 {
        observed.checked_add(Duration::from_secs_f64(lead))
    } else {
        observed.checked_sub(Duration::from_secs_f64(-lead))
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos", test))]
pub(crate) fn clamp_presented_at(
    presented: Option<Instant>,
    completed_at: Instant,
) -> Option<Instant> {
    presented.map(|presented| presented.max(completed_at))
}

#[cfg(any(windows, test))]
const MAX_QPC_LEAD_SECS: f64 = 1.0;

#[cfg(any(windows, test))]
pub(crate) fn qpc_instant(
    presented_qpc: i64,
    anchor_qpc: i64,
    anchor: Instant,
    now: Instant,
    frequency: i64,
) -> Option<Instant> {
    if frequency <= 0 {
        return None;
    }
    let ticks = presented_qpc.checked_sub(anchor_qpc)?;
    let seconds = ticks as f64 / frequency as f64;
    if !seconds.is_finite() {
        return None;
    }
    let instant = if seconds >= 0.0 {
        anchor.checked_add(Duration::from_secs_f64(seconds))?
    } else {
        anchor.checked_sub(Duration::from_secs_f64(-seconds))?
    };
    let age = now.saturating_duration_since(instant);
    (instant <= now && age <= Duration::from_secs_f64(MAX_QPC_LEAD_SECS)).then_some(instant)
}

#[cfg(any(windows, test))]
pub(crate) fn refresh_period_qpc(
    previous: (u32, i64),
    current: (u32, i64),
    frequency: i64,
) -> Option<i64> {
    if frequency <= 0 {
        return None;
    }
    let count_delta = current.0.checked_sub(previous.0)?;
    let qpc_delta = current.1.checked_sub(previous.1)?;
    if count_delta == 0 || qpc_delta <= 0 {
        return None;
    }
    let period = qpc_delta / i64::from(count_delta);
    let seconds = period as f64 / frequency as f64;
    if !(0.001..=0.1).contains(&seconds) {
        return None;
    }
    Some(period)
}

#[cfg(any(windows, test))]
pub(crate) fn presented_qpc_from_frame_statistics(
    sync_refresh_count: u32,
    present_refresh_count: u32,
    sync_qpc_time: i64,
    period_qpc: Option<i64>,
) -> Option<i64> {
    let lag = sync_refresh_count.checked_sub(present_refresh_count)?;
    let offset = match (lag, period_qpc) {
        (0, _) => 0,
        (_, Some(period)) if period > 0 => i64::from(lag).checked_mul(period)?,
        _ => return None,
    };
    let presented = sync_qpc_time.checked_sub(offset)?;
    (presented > 0).then_some(presented)
}

#[cfg(any(target_os = "linux", test))]
const MAX_PRESENTATION_DELTA_SECS: f64 = 1.0;

#[cfg(any(target_os = "linux", test))]
pub(crate) const CLOCK_REALTIME: i32 = 0;
#[cfg(any(target_os = "linux", test))]
pub(crate) const CLOCK_MONOTONIC: i32 = 1;

#[cfg(any(target_os = "linux", test))]
pub(crate) fn presented_event_instant(
    tv_sec_hi: u32,
    tv_sec_lo: u32,
    tv_nsec: u32,
    clock_id: i32,
    clock_anchor_secs: f64,
    anchor: Instant,
    now: Instant,
) -> Option<Instant> {
    if !matches!(clock_id, CLOCK_REALTIME | CLOCK_MONOTONIC) {
        return None;
    }
    if tv_nsec > 999_999_999 || !clock_anchor_secs.is_finite() {
        return None;
    }
    let secs = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
    let presented = secs as f64 + f64::from(tv_nsec) / 1e9;
    let delta = presented - clock_anchor_secs;
    if !delta.is_finite() || delta.abs() > MAX_PRESENTATION_DELTA_SECS {
        return None;
    }
    let instant = if delta >= 0.0 {
        anchor.checked_add(Duration::from_secs_f64(delta))?
    } else {
        anchor.checked_sub(Duration::from_secs_f64(-delta))?
    };
    (instant <= now).then_some(instant)
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentationOutcome {
    Presented(Instant),
    Discarded,
}

#[cfg(any(target_os = "linux", test))]
const MAX_PRESENTATION_EVENTS: usize = 16;

#[cfg(any(target_os = "linux", test))]
pub(crate) struct PresentationInbox {
    events: std::sync::Mutex<VecDeque<(FrameIndex, PresentationOutcome)>>,
    waker: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    dropped: std::sync::atomic::AtomicU64,
}

#[cfg(any(target_os = "linux", test))]
impl PresentationInbox {
    pub(crate) fn new() -> Self {
        Self {
            events: std::sync::Mutex::new(VecDeque::with_capacity(MAX_PRESENTATION_EVENTS)),
            waker: std::sync::Mutex::new(None),
            dropped: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_waker(&self, waker: std::sync::Arc<dyn Fn() + Send + Sync>) {
        *self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(waker);
    }

    pub(crate) fn note(&self, index: FrameIndex, outcome: PresentationOutcome) {
        {
            let mut events = self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if events.iter().any(|(queued, _)| *queued == index) {
                return;
            }
            if events.len() >= MAX_PRESENTATION_EVENTS {
                let _ = events.pop_front();
                self.dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            events.push_back((index, outcome));
        }
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(waker) = waker {
            waker();
        }
    }

    pub(crate) fn take(&self, index: FrameIndex) -> Option<PresentationOutcome> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let position = events.iter().position(|(queued, _)| *queued == index)?;
        events.remove(position).map(|(_, outcome)| outcome)
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[cfg(test)]
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(any(target_os = "macos", test))]
struct SubmittedFrame {
    index: FrameIndex,
    stats: RenderStats,
    samples: Vec<GpuTimerSample>,
    published_at: Instant,
    tick: u64,
    completed_at: Option<Instant>,
    presented_at: Option<Instant>,
    presented: bool,
}

#[cfg(any(target_os = "macos", test))]
const MAX_FEEDBACK_FRAMES: usize = MAX_FRAMES_IN_FLIGHT + 1;

#[cfg(any(target_os = "macos", test))]
pub(crate) struct FeedbackQueue {
    submitted: VecDeque<SubmittedFrame>,
    dropped: u64,
    abandoned: VecDeque<FrameIndex>,
}

#[cfg(any(target_os = "macos", test))]
impl FeedbackQueue {
    pub(crate) fn new() -> Self {
        Self {
            submitted: VecDeque::with_capacity(MAX_FEEDBACK_FRAMES),
            dropped: 0,
            abandoned: VecDeque::new(),
        }
    }

    pub(crate) fn push(
        &mut self,
        index: FrameIndex,
        stats: RenderStats,
        samples: Vec<GpuTimerSample>,
        published_at: Instant,
        tick: u64,
        presented_at: Option<Instant>,
    ) {
        if self.submitted.len() >= MAX_FEEDBACK_FRAMES {
            let dropped = self.submitted.pop_front();
            self.dropped = self.dropped.saturating_add(1);
            if self.dropped == 1 {
                eprintln!(
                    "kytrix: feedback queue hit {MAX_FEEDBACK_FRAMES} frames without a display-link tick; \
                     dropping the oldest feedback (subsequent drops are only counted)"
                );
            }
            if let Some(frame) = dropped
                && frame.completed_at.is_none()
            {
                self.abandoned.push_back(frame.index);
            }
        }
        self.submitted.push_back(SubmittedFrame {
            index,
            stats,
            samples,
            published_at,
            tick,
            completed_at: None,
            presented_at,
            presented: false,
        });
    }

    pub(crate) fn complete(&mut self, index: FrameIndex, sample: Option<GpuTimerSample>) {
        let position = self.submitted.iter().position(|frame| frame.index == index);
        let Some(position) = position else {
            if self.abandoned.front() == Some(&index) {
                let _ = self.abandoned.pop_front();
                return;
            }
            debug_assert!(
                false,
                "completion for frame {} without a submission",
                index.get()
            );
            return;
        };
        let frame = &mut self.submitted[position];
        if let Some(sample) = sample {
            frame.samples.push(sample);
        }
        frame.completed_at = Some(Instant::now());
    }

    pub(crate) fn observe_tick(&mut self, latest: u64) {
        for frame in &mut self.submitted {
            if !frame.presented && frame.tick < latest {
                frame.presented = true;
            }
        }
    }

    pub(crate) fn awaiting_tick(&self) -> Option<u64> {
        self.submitted
            .iter()
            .find(|frame| !frame.presented)
            .map(|frame| frame.tick)
    }

    pub(crate) fn pop_ready(
        &mut self,
        frontier: FrameIndex,
        mut publish: impl FnMut(FrameFeedback),
    ) {
        while let Some(front) = self.submitted.front() {
            if front.index > frontier {
                break;
            }
            let Some(completed_at) = front.completed_at else {
                debug_assert!(false, "completion frontier passed an incomplete frame");
                break;
            };
            if !front.presented {
                break;
            }
            let presented_at = clamp_presented_at(front.presented_at, completed_at);
            let frame = self.submitted.pop_front().expect("front exists");
            publish(FrameFeedback {
                index: frame.index,
                stats: frame.stats,
                samples: frame.samples,
                latency: completed_at.saturating_duration_since(frame.published_at),
                completed_at,
                presented_at,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        CLOCK_MONOTONIC, CLOCK_REALTIME, FeedbackQueue, IDLE_GRACE, MAX_PRESENTATION_EVENTS,
        MAX_TARGET_LEAD, PacingGate, PresentationInbox, PresentationOutcome, clamp_presented_at,
        pacing_work, presentation_instant, presented_event_instant,
        presented_qpc_from_frame_statistics, qpc_instant, refresh_period_qpc,
    };
    use crate::{render::RenderStats, window::FrameIndex};

    fn index(value: u32) -> FrameIndex {
        FrameIndex::from_raw(value)
    }

    fn stats(draw_calls: u32) -> RenderStats {
        RenderStats {
            draw_calls,
            instances: draw_calls * 10,
            geometry_uploaded_bytes: 0,
        }
    }

    #[test]
    fn pacing_work_covers_every_reason_to_keep_running() {
        assert!(pacing_work(true, false, 0, false), "连续帧必须跑");
        assert!(pacing_work(false, true, 0, false), "有未取走的帧请求必须跑");
        assert!(pacing_work(false, false, 1, false), "有在飞帧必须跑");
        assert!(
            pacing_work(false, false, 0, true),
            "等上屏证据时不能暂停：tick 是证据的唯一来源"
        );
        assert!(!pacing_work(false, false, 0, false), "空闲：没有工作");
    }

    #[test]
    fn isolated_updates_do_not_pause_the_display_link() {
        let start = Instant::now();
        let at = |millis: u64| start + Duration::from_millis(millis);
        let mut gate = PacingGate::new();
        assert!(gate.update(false, at(0)), "从未工作过 ⇒ 暂停");

        assert!(!gate.update(true, at(100)), "有工作必须跑");
        assert!(
            !gate.update(false, at(116)),
            "滞回窗口内（{IDLE_GRACE:?}）不得暂停"
        );
        assert!(!gate.update(true, at(300)), "孤立更新必须直接出帧");
        assert!(!gate.update(false, at(316)));
        assert!(!gate.update(true, at(500)));
        assert_eq!(
            gate.pauses(),
            0,
            "3 次孤立更新之间不得出现 pause→resume 往返（旧实现 = 3 次，每次白付一帧）"
        );

        let idle_deadline = at(500) + IDLE_GRACE;
        assert!(
            !gate.update(false, idle_deadline - Duration::from_millis(1)),
            "滞回窗口的最后一毫秒仍然必须跑"
        );
        assert!(gate.update(false, idle_deadline), "窗口结束 ⇒ 暂停");
        assert_eq!(gate.pauses(), 1, "暂停转换只允许计一次");
        assert!(gate.update(false, at(2_000)), "已暂停就继续保持暂停");
        assert_eq!(gate.pauses(), 1, "保持暂停不得重复计数");

        assert!(!gate.update(true, at(2_100)), "新工作必须恢复 display link");
        assert!(!gate.update(false, at(2_116)), "恢复之后同样有滞回窗口");
        assert_eq!(gate.pauses(), 1, "恢复不是暂停，不计入 pauses");
    }

    #[test]
    fn the_pacing_decision_costs_nanoseconds() {
        const ROUNDS: u32 = 1_000_000;
        let start = Instant::now();
        let mut gate = PacingGate::new();
        let began = Instant::now();
        for round in 0..ROUNDS {
            let work = round % 64 == 0;
            std::hint::black_box(
                gate.update(work, start + Duration::from_micros(u64::from(round))),
            );
        }
        let per_round = began.elapsed() / ROUNDS;
        eprintln!("pacing_gate: {per_round:?} / decision（{ROUNDS} 轮）");
        assert!(
            per_round < Duration::from_micros(1),
            "单次暂停裁决必须远低于 1 µs（实际 {per_round:?}）"
        );
    }

    #[test]
    fn the_target_presentation_time_is_converted_into_the_monotonic_clock() {
        let observed = Instant::now();
        let now_host = 500.0;
        let lead = presentation_instant(now_host + 0.008, observed, now_host).expect("instant");
        assert_eq!(
            lead.saturating_duration_since(observed),
            Duration::from_millis(8),
            "本 vsync 的目标时刻在未来：必须往后推 8 ms"
        );
        let late = presentation_instant(now_host - 0.010, observed, now_host).expect("instant");
        assert_eq!(
            observed.saturating_duration_since(late),
            Duration::from_millis(10),
            "已经过去的排定时刻：往前推 10 ms"
        );
        assert!(presentation_instant(0.0, observed, now_host).is_none());
        assert!(presentation_instant(f64::NAN, observed, now_host).is_none());
        assert!(presentation_instant(f64::INFINITY, observed, now_host).is_none());
        assert!(
            presentation_instant(
                now_host + MAX_TARGET_LEAD.as_secs_f64() * 2.0,
                observed,
                now_host
            )
            .is_none(),
            "超过一个合理提前量的目标时刻是垃圾值"
        );
    }

    #[test]
    fn the_tick_inferred_presentation_fills_presented_at_but_not_latency() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now() - Duration::from_millis(100);
        let target = Instant::now() + Duration::from_millis(3);
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            published_at,
            7,
            Some(target),
        );
        feedback.complete(index(1), None);
        assert_eq!(feedback.awaiting_tick(), Some(7), "在等更新的 tick");
        let mut published = Vec::new();
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert!(published.is_empty(), "没有上屏证据时不得发布");
        feedback.observe_tick(8);
        assert_eq!(feedback.awaiting_tick(), None, "证据已到");
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].latency,
            published[0]
                .completed_at
                .saturating_duration_since(published_at),
            "latency = 发布 → GPU 完成"
        );
        assert_eq!(
            published[0].presented_at,
            Some(target.max(published[0].completed_at)),
            "presented_at 是 tick 推断的上屏时刻（不小于 GPU 完成）"
        );
        assert!(
            published[0].presentation_is_consistent(),
            "共享契约：presented_at 不早于 completed_at"
        );
    }

    #[test]
    fn tick_evidence_marks_only_frames_from_older_ticks_as_presented() {
        let mut feedback = FeedbackQueue::new();
        let base = Instant::now() - Duration::from_millis(50);
        feedback.push(index(1), stats(1), Vec::new(), base, 10, Some(base));
        feedback.push(index(2), stats(2), Vec::new(), base, 11, Some(base));
        feedback.push(index(3), stats(3), Vec::new(), base, 11, Some(base));
        feedback.observe_tick(10);
        assert_eq!(feedback.awaiting_tick(), Some(10), "同一 tick 不算证据");
        feedback.observe_tick(11);
        assert_eq!(
            feedback.awaiting_tick(),
            Some(11),
            "tick 10 的帧已上屏；tick 11 的帧还在等下一个 tick"
        );
        assert!(feedback.submitted[0].presented);
        assert!(!feedback.submitted[1].presented);
        feedback.observe_tick(12);
        assert_eq!(feedback.awaiting_tick(), None, "全部拿到上屏证据");
    }

    #[test]
    fn an_unusable_target_time_publishes_a_none_presentation() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now();
        feedback.push(index(1), stats(1), Vec::new(), published_at, 4, None);
        feedback.complete(index(1), None);
        feedback.observe_tick(5);
        let mut published = Vec::new();
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].presented_at, None,
            "拿不到可信上屏时刻就留空，不造假"
        );
        assert!(published[0].presentation_is_consistent(), "None 是合法值");
    }

    #[test]
    fn an_early_target_is_clamped_to_the_gpu_completion() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now() - Duration::from_millis(20);
        let early_target = published_at + Duration::from_millis(1);
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            published_at,
            2,
            Some(early_target),
        );
        feedback.complete(index(1), None);
        feedback.observe_tick(3);
        let mut published = Vec::new();
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        assert_eq!(
            published[0].presented_at,
            Some(published[0].completed_at),
            "排定时刻早于 GPU 完成时按完成时刻取下界"
        );
        assert!(published[0].presentation_is_consistent());
        assert_eq!(
            clamp_presented_at(Some(early_target), published[0].completed_at),
            Some(published[0].completed_at)
        );
    }

    #[test]
    fn no_frame_is_published_without_tick_evidence() {
        let mut feedback = FeedbackQueue::new();
        let published_at = Instant::now() - Duration::from_millis(80);
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            published_at,
            5,
            Some(published_at),
        );
        feedback.complete(index(1), None);
        assert_eq!(feedback.awaiting_tick(), Some(5), "在等更新的 tick");
        let mut published = Vec::new();
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert!(published.is_empty(), "等上屏证据期间不发布");
        assert_eq!(feedback.submitted.len(), 1);
        feedback.observe_tick(6);
        feedback.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
        assert!(feedback.submitted.is_empty(), "发布后队列清空");
    }

    #[test]
    fn tick_evidence_without_frames_is_a_no_op() {
        let mut feedback = FeedbackQueue::new();
        feedback.observe_tick(99);
        assert!(feedback.submitted.is_empty());
        assert_eq!(feedback.awaiting_tick(), None);
    }

    #[test]
    fn out_of_order_completions_publish_in_submission_order() {
        let mut feedback = FeedbackQueue::new();
        let base = Instant::now();
        feedback.push(
            index(1),
            stats(1),
            Vec::new(),
            base - Duration::from_millis(100),
            21,
            Some(base),
        );
        feedback.push(index(2), stats(2), Vec::new(), base, 21, Some(base));
        feedback.push(index(3), stats(3), Vec::new(), base, 21, Some(base));
        feedback.complete(index(2), None);
        feedback.complete(index(3), None);
        feedback.complete(index(1), None);
        feedback.observe_tick(22);
        let mut published = Vec::new();
        feedback.pop_ready(index(3), |frame| published.push(frame));

        assert_eq!(
            published
                .iter()
                .map(|frame| frame.index.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "反馈按提交顺序发布"
        );
        assert_eq!(published[0].stats.draw_calls, 1);
        assert_eq!(published[1].stats.draw_calls, 2);
        assert_eq!(published[2].stats.draw_calls, 3);
        assert!(
            published[0].latency >= Duration::from_millis(90),
            "第 1 帧的 latency 用自己的发布基准，不是更旧帧的完成时刻"
        );
        assert!(
            published[1].latency < Duration::from_millis(90),
            "第 2 帧不能被更旧帧的 latency 污染"
        );
        assert!(feedback.submitted.is_empty(), "发布后队列清空");
    }

    #[test]
    fn the_feedback_queue_is_bounded_without_ticks_and_counts_drops() {
        let mut queue = FeedbackQueue::new();
        let now = Instant::now();
        let total = MAX_FEEDBACK_ROUNDS;
        for value in 0..total {
            queue.push(
                index(value + 1),
                stats(value + 1),
                Vec::new(),
                now,
                1,
                Some(now),
            );
        }
        assert_eq!(
            queue.submitted.len(),
            super::MAX_FEEDBACK_FRAMES,
            "长期不 tick 时队列必须有硬上限（旧实现无界增长）"
        );
        let dropped = total - super::MAX_FEEDBACK_FRAMES as u32;
        assert_eq!(queue.dropped, u64::from(dropped), "丢弃次数必须计数");
        assert_eq!(
            queue.abandoned.len(),
            dropped as usize,
            "未完成即被丢弃的帧要记下（完成回调晚到不 panic）"
        );
        assert_eq!(
            queue.submitted.front().expect("front").index.get(),
            dropped + 1
        );
        assert_eq!(queue.submitted.back().expect("back").index.get(), total);
        let first_abandoned = queue.abandoned.front().copied().expect("abandoned");
        queue.complete(first_abandoned, None);
        assert_eq!(queue.abandoned.len(), dropped as usize - 1);

        let mut healthy = FeedbackQueue::new();
        healthy.push(index(1), stats(1), Vec::new(), now, 1, Some(now));
        healthy.complete(index(1), None);
        healthy.observe_tick(2);
        assert_eq!(healthy.dropped, 0);
        let mut published = Vec::new();
        healthy.pop_ready(index(1), |frame| published.push(frame));
        assert_eq!(published.len(), 1);
    }

    const MAX_FEEDBACK_ROUNDS: u32 = (super::MAX_FEEDBACK_FRAMES as u32) * 4;

    #[test]
    fn the_qpc_counter_is_mapped_onto_the_monotonic_clock() {
        const FREQUENCY: i64 = 10_000_000;
        let anchor = Instant::now() - Duration::from_millis(100);
        let now = Instant::now();
        let anchor_qpc = 1_000_000_000;
        let ahead = qpc_instant(
            anchor_qpc + FREQUENCY / 60,
            anchor_qpc,
            anchor,
            now,
            FREQUENCY,
        )
        .expect("qpc ahead");
        assert_eq!(
            ahead.saturating_duration_since(anchor),
            Duration::from_nanos(16_666_600),
            "一个 60 Hz vblank 必须映射成 16.6666 ms"
        );
        let behind = qpc_instant(
            anchor_qpc - FREQUENCY / 200,
            anchor_qpc,
            anchor,
            now,
            FREQUENCY,
        )
        .expect("qpc behind");
        assert_eq!(
            anchor.saturating_duration_since(behind),
            Duration::from_millis(5)
        );
        assert!(qpc_instant(anchor_qpc, anchor_qpc, anchor, now, 0).is_none());
        assert!(
            qpc_instant(
                anchor_qpc + i64::MAX / 2,
                anchor_qpc,
                anchor,
                now,
                FREQUENCY
            )
            .is_none()
        );
        assert!(qpc_instant(i64::MIN, anchor_qpc, anchor, now, FREQUENCY).is_none());
        assert!(
            qpc_instant(
                anchor_qpc,
                anchor_qpc,
                now - Duration::from_secs(2),
                now,
                FREQUENCY
            )
            .is_none(),
            "离采样时刻 2 s 的「上屏证据」不是本拍的统计"
        );
        let future = qpc_instant(
            anchor_qpc + FREQUENCY / 10,
            anchor_qpc,
            now - Duration::from_millis(50),
            now,
            FREQUENCY,
        );
        assert!(future.is_none(), "映射到采样时刻之后的呈现时刻不可信");
    }

    #[test]
    fn an_anchor_sixty_seconds_old_still_maps_onto_the_monotonic_clock() {
        const FREQUENCY: i64 = 10_000_000;
        let now = Instant::now();
        let anchor = now - Duration::from_secs(60);
        let anchor_qpc = 1_000_000_000;
        let instant = qpc_instant(
            anchor_qpc + FREQUENCY * 60 - FREQUENCY / 200,
            anchor_qpc,
            anchor,
            now,
            FREQUENCY,
        )
        .expect("锚点早 60 s 仍必须能映射（RA-1 回归）");
        assert_eq!(
            instant.saturating_duration_since(anchor),
            Duration::from_secs(59) + Duration::from_millis(995)
        );
        assert!(instant <= now);
        let anchor = now - Duration::from_secs(600);
        let instant = qpc_instant(
            anchor_qpc + FREQUENCY * 600 - FREQUENCY / 500,
            anchor_qpc,
            anchor,
            now,
            FREQUENCY,
        )
        .expect("锚点早 10 min 仍必须能映射");
        assert_eq!(
            instant.saturating_duration_since(anchor),
            Duration::from_secs(599) + Duration::from_millis(998)
        );
    }

    #[test]
    fn the_refresh_period_is_derived_from_consecutive_frame_statistics() {
        const FREQUENCY: i64 = 10_000_000;
        let period = FREQUENCY / 60;
        assert_eq!(
            refresh_period_qpc((100, 1_000_000), (101, 1_000_000 + period), FREQUENCY),
            Some(period)
        );
        assert!(refresh_period_qpc((100, 1_000_000), (100, 1_016_667), FREQUENCY).is_none());
        assert!(refresh_period_qpc((100, 1_016_667), (101, 1_000_000), FREQUENCY).is_none());
        assert!(refresh_period_qpc((100, 0), (101, 1), FREQUENCY).is_none());
        assert!(
            refresh_period_qpc((100, 0), (101, FREQUENCY / 5 + 1), FREQUENCY).is_none(),
            "5 Hz 不在合理刷新范围内"
        );
    }

    #[test]
    fn frame_statistics_yield_the_presentation_lower_bound() {
        const PERIOD: i64 = 166_667;
        assert_eq!(
            presented_qpc_from_frame_statistics(500, 500, 9_000_000, None),
            Some(9_000_000)
        );
        assert_eq!(
            presented_qpc_from_frame_statistics(500, 498, 9_000_000, Some(PERIOD)),
            Some(9_000_000 - 2 * PERIOD)
        );
        assert!(presented_qpc_from_frame_statistics(498, 500, 9_000_000, None).is_none());
        assert!(presented_qpc_from_frame_statistics(500, 498, 9_000_000, None).is_none());
        assert!(presented_qpc_from_frame_statistics(500, 498, 9_000_000, Some(0)).is_none());
        assert!(
            presented_qpc_from_frame_statistics(500, 498, i64::MIN + 1, Some(PERIOD)).is_none()
        );
        assert!(presented_qpc_from_frame_statistics(2, 0, 1, Some(PERIOD)).is_none());
    }

    #[test]
    fn the_presentation_clock_is_mapped_onto_the_monotonic_clock() {
        let anchor = Instant::now() - Duration::from_millis(100);
        let now = Instant::now();
        let clock_anchor = 1_000.5_f64;
        let instant = presented_event_instant(
            0,
            1_000,
            600_000_000,
            CLOCK_MONOTONIC,
            clock_anchor,
            anchor,
            now,
        )
        .expect("presented instant");
        assert_eq!(
            instant.saturating_duration_since(anchor),
            Duration::from_millis(100)
        );
        let earlier = presented_event_instant(
            0,
            1_000,
            450_000_000,
            CLOCK_REALTIME,
            clock_anchor,
            anchor,
            now,
        )
        .expect("presented instant before anchor");
        assert_eq!(
            anchor.saturating_duration_since(earlier),
            Duration::from_millis(50)
        );
        let high = presented_event_instant(1, 0, 0, CLOCK_MONOTONIC, 4_294_967_296.0, anchor, now)
            .expect("hi/lo seconds");
        assert_eq!(high, anchor);
        for (clock, nsec, clock_anchor, secs) in [
            (4, 0, 1_000.5, 1_000_u32),
            (CLOCK_MONOTONIC, 1_000_000_000, 1_000.5, 1_000),
            (CLOCK_MONOTONIC, 0, f64::NAN, 1_000),
            (CLOCK_MONOTONIC, 0, 1_000.5, 5_000),
        ] {
            assert!(
                presented_event_instant(0, secs, nsec, clock, clock_anchor, anchor, now).is_none(),
                "clock={clock} nsec={nsec} anchor={clock_anchor} secs={secs} 必须是 None"
            );
        }
        assert!(
            presented_event_instant(
                0,
                1_000,
                900_000_000,
                CLOCK_MONOTONIC,
                1_000.5,
                now - Duration::from_millis(100),
                now
            )
            .is_none(),
            "晚于采样时刻的呈现时间戳不可信"
        );
    }

    #[test]
    fn the_presentation_inbox_maps_out_of_order_feedback_by_frame() {
        let inbox = PresentationInbox::new();
        let presented = Instant::now() - Duration::from_millis(8);
        inbox.note(index(3), PresentationOutcome::Presented(presented));
        inbox.note(
            index(1),
            PresentationOutcome::Presented(presented - Duration::from_millis(1)),
        );
        inbox.note(index(2), PresentationOutcome::Discarded);
        assert_eq!(inbox.pending(), 3);
        assert_eq!(
            inbox.take(index(1)),
            Some(PresentationOutcome::Presented(
                presented - Duration::from_millis(1)
            ))
        );
        assert_eq!(inbox.take(index(2)), Some(PresentationOutcome::Discarded));
        assert_eq!(
            inbox.take(index(3)),
            Some(PresentationOutcome::Presented(presented))
        );
        assert_eq!(inbox.take(index(3)), None, "终态只取一次");
        inbox.note(index(4), PresentationOutcome::Presented(presented));
        inbox.note(index(4), PresentationOutcome::Discarded);
        assert_eq!(
            inbox.take(index(4)),
            Some(PresentationOutcome::Presented(presented))
        );
    }

    #[test]
    fn the_presentation_inbox_is_bounded_and_counts_drops() {
        let inbox = PresentationInbox::new();
        let presented = Instant::now();
        for value in 0..(MAX_PRESENTATION_EVENTS as u32 + 4) {
            inbox.note(index(value + 1), PresentationOutcome::Presented(presented));
        }
        assert_eq!(inbox.pending(), MAX_PRESENTATION_EVENTS);
        assert_eq!(inbox.dropped(), 4);
        assert_eq!(inbox.take(index(1)), None, "最旧的被丢弃");
        assert_eq!(
            inbox.take(index(5)),
            Some(PresentationOutcome::Presented(presented)),
            "丢的是最旧的 4 条，保留 5..=20"
        );
        let healthy = PresentationInbox::new();
        healthy.note(index(1), PresentationOutcome::Discarded);
        assert_eq!(healthy.pending(), 1);
        assert_eq!(healthy.dropped(), 0);
    }

    #[test]
    fn the_presentation_inbox_wakes_the_gpu_thread() {
        use std::sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        };
        let inbox = PresentationInbox::new();
        let wakes = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&wakes);
        inbox.set_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::AcqRel);
        }));
        inbox.note(index(1), PresentationOutcome::Discarded);
        inbox.note(index(2), PresentationOutcome::Presented(Instant::now()));
        assert_eq!(wakes.load(Ordering::Acquire), 2);
    }

    #[test]
    fn the_presentation_clock_rejects_timestamps_outside_the_plausible_window() {
        let anchor = Instant::now();
        let now = anchor;
        let clock_anchor = 2_000.0_f64;
        assert_eq!(
            presented_event_instant(0, 1_999, 0, CLOCK_MONOTONIC, clock_anchor, anchor, now),
            Some(anchor - Duration::from_secs(1)),
            "边界内（-1 s）必须接受"
        );
        for secs in [1_998_u32, 2_002] {
            assert!(
                presented_event_instant(0, secs, 0, CLOCK_MONOTONIC, clock_anchor, anchor, now)
                    .is_none(),
                "secs={secs} 距锚点超过 1 s 必须拒绝"
            );
        }
        assert!(
            presented_event_instant(
                0,
                1_999,
                999_999_999,
                CLOCK_MONOTONIC,
                clock_anchor,
                anchor,
                now
            )
            .is_some(),
            "999_999_999 ns 合法"
        );
        assert!(
            presented_event_instant(
                0,
                1_999,
                1_000_000_000,
                CLOCK_MONOTONIC,
                clock_anchor,
                anchor,
                now
            )
            .is_none(),
            "nsec ≥ 1e9 非法"
        );
        assert!(
            presented_event_instant(
                0,
                2_000,
                500_000_000,
                CLOCK_MONOTONIC,
                clock_anchor,
                anchor,
                now
            )
            .is_none(),
            "晚于采样时刻的呈现时间戳不可信"
        );
    }

    #[test]
    fn a_presentation_timestamp_older_than_the_gpu_completion_is_clamped() {
        use crate::window::{FramePresentation, Window, WindowId, WindowOptions};
        let mut window = Window::new(WindowId::for_test(1), WindowOptions::default());
        let published = Instant::now();
        let completed = published + Duration::from_millis(2);
        let frame = index(1);
        window.note_frame_presentation(FramePresentation {
            index: frame,
            published_at: published,
            completed_at: completed,
            presented_at: None,
            latency: Duration::from_millis(2),
        });
        let early = completed
            .checked_sub(Duration::from_millis(5))
            .expect("测试时间戳");
        window.note_frame_presented(frame, Some(early));
        assert_eq!(
            window
                .last_frame_presentation()
                .expect("本帧还在历史里")
                .presented_at,
            Some(completed),
            "presented 不得早于 GPU 完成（下界收口）"
        );
        let later = completed + Duration::from_millis(3);
        window.note_frame_presented(frame, Some(later));
        assert_eq!(
            window
                .last_frame_presentation()
                .expect("本帧还在历史里")
                .presented_at,
            Some(later)
        );
        window.note_frame_presented(frame, None);
        assert_eq!(
            window
                .last_frame_presentation()
                .expect("本帧还在历史里")
                .presented_at,
            None
        );
    }
}
