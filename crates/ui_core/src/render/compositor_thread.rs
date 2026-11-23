use std::{
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use smol_str::SmolStr;

use crate::{
    error::EngineError,
    render::{
        compose::{ComposeStats, WindowComposer},
        delta::{SceneDelta, absorb},
        frame::{FrameRequest, FrameSink},
    },
    window::WindowId,
};

pub(crate) const COMPOSITOR_THREAD_NAME: &str = "kytrix-compositor";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompositorCounters {
    pub frames: u64,
    pub absorbed: u64,
    pub encoded_lanes: usize,
    pub reused_segments: usize,
    pub inbox_latency: Duration,
    pub encode: Duration,
}

impl CompositorCounters {
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn encoded_on_compositor(&self) -> bool {
        self.encoded_lanes > 0
    }
}

pub(crate) struct CompositorInbox {
    state: Mutex<InboxState>,
    condvar: Condvar,
}

struct InboxState {
    pending: Vec<(WindowId, SceneDelta)>,
    absorbed: u64,
    closed: bool,
}

impl CompositorInbox {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(InboxState {
                pending: Vec::new(),
                absorbed: 0,
                closed: false,
            }),
            condvar: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, InboxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn publish(&self, window: WindowId, delta: SceneDelta) -> Option<bool> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        let covered = match state.pending.iter_mut().find(|(id, _)| *id == window) {
            Some((_, pending)) => {
                let previous = std::mem::replace(pending, delta);
                absorb(previous, pending);
                state.absorbed = state.absorbed.saturating_add(1);
                true
            }
            None => {
                state.pending.push((window, delta));
                false
            }
        };
        drop(state);
        self.condvar.notify_all();
        Some(covered)
    }

    pub(crate) fn take_blocking(&self) -> Option<Vec<(WindowId, SceneDelta)>> {
        let mut state = self.lock();
        loop {
            if state.closed {
                return None;
            }
            if !state.pending.is_empty() {
                return Some(std::mem::take(&mut state.pending));
            }
            state = self
                .condvar
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    #[cfg(test)]
    pub(crate) fn has_pending(&self) -> bool {
        !self.lock().pending.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn absorbed(&self) -> u64 {
        self.lock().absorbed
    }

    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.condvar.notify_all();
    }

    pub(crate) fn reopen(&self) {
        self.lock().closed = false;
    }

    fn clear(&self) {
        self.lock().pending.clear();
    }
}

struct CompositorShared {
    inbox: CompositorInbox,
    sinks: Mutex<ahash::HashMap<WindowId, Arc<dyn FrameSink>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    counters: Mutex<CompositorCounters>,
    error: Mutex<Option<EngineError>>,
    running: AtomicBool,
}

impl CompositorShared {
    fn lock_sinks(&self) -> MutexGuard<'_, ahash::HashMap<WindowId, Arc<dyn FrameSink>>> {
        self.sinks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn take_batch(&self) -> Option<Vec<(WindowId, SceneDelta)>> {
        self.inbox.take_blocking()
    }

    fn publish_frame(&self, window: WindowId, request: FrameRequest) -> bool {
        let sink = self.lock_sinks().get(&window).map(Arc::clone);
        match sink {
            Some(sink) => sink.submit(request),
            None => false,
        }
    }

    fn record(&self, frame: (ComposeStats, Duration, Duration)) {
        let (stats, inbox_latency, encode) = frame;
        let mut counters = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
        counters.frames = counters.frames.saturating_add(1);
        counters.encoded_lanes = stats.encoded_lanes;
        counters.reused_segments = stats.reused_segments;
        counters.inbox_latency = inbox_latency;
        counters.encode = encode;
    }

    fn take_error(&self) -> Option<EngineError> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn set_error(&self, error: EngineError) {
        let mut slot = self.error.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(error);
        }
    }
}

#[derive(Clone)]
pub(crate) struct CompositorHandle {
    shared: Arc<CompositorShared>,
}

impl CompositorHandle {
    fn new() -> Self {
        Self {
            shared: Arc::new(CompositorShared {
                inbox: CompositorInbox::new(),
                sinks: Mutex::new(ahash::HashMap::default()),
                join: Mutex::new(None),
                counters: Mutex::new(CompositorCounters::default()),
                error: Mutex::new(None),
                running: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn register(self: &Arc<Self>, window: WindowId, sink: Arc<dyn FrameSink>) {
        self.shared.lock_sinks().insert(window, sink);
        self.ensure_running();
    }

    pub(crate) fn unregister(&self, window: WindowId) {
        let empty = {
            let mut sinks = self.shared.lock_sinks();
            sinks.remove(&window);
            sinks.is_empty()
        };
        if empty {
            self.shutdown();
        }
    }

    pub(crate) fn publish(&self, delta: SceneDelta) -> Result<(), EngineError> {
        let window = delta.header.window;
        if self.shared.inbox.publish(window, delta).is_some() {
            return Ok(());
        }
        let detail = SmolStr::new_static("compositor inbox closed while a window is registered");
        self.shared.set_error(EngineError::FramePipeline {
            detail: detail.clone(),
        });
        Err(EngineError::FramePipeline { detail })
    }

    pub(crate) fn take_error(&self) -> Option<EngineError> {
        self.shared.take_error()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn counters(&self) -> CompositorCounters {
        *self
            .shared
            .counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    pub(crate) fn supervise(self: &Arc<Self>) -> Result<bool, EngineError> {
        if !self.shared.running.load(Ordering::Acquire) {
            return Ok(false);
        }
        let finished = self
            .shared
            .join
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(JoinHandle::is_finished);
        if !finished {
            return Ok(false);
        }
        self.shared.inbox.clear();
        self.shared.running.store(false, Ordering::Release);
        self.ensure_running();
        if !self.shared.running.load(Ordering::Acquire) {
            return Err(EngineError::FramePipeline {
                detail: SmolStr::new_static(
                    "compositor thread exited unexpectedly and could not be restarted",
                ),
            });
        }
        Ok(true)
    }

    fn ensure_running(self: &Arc<Self>) {
        if self.shared.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut join = self
            .shared
            .join
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(handle) = join.take() {
            let _ = handle.join();
        }
        self.shared.inbox.reopen();
        let shared = Arc::clone(&self.shared);
        match thread::Builder::new()
            .name(COMPOSITOR_THREAD_NAME.into())
            .spawn(move || run(shared))
        {
            Ok(handle) => *join = Some(handle),
            Err(error) => {
                self.shared.running.store(false, Ordering::Release);
                eprintln!("kytrix: compositor thread spawn failed: {error}");
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        self.shared.inbox.close();
        let handle = self
            .shared
            .join
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
        self.shared.running.store(false, Ordering::Release);
    }
}

pub(crate) fn compositor() -> &'static Arc<CompositorHandle> {
    static HANDLE: std::sync::OnceLock<Arc<CompositorHandle>> = std::sync::OnceLock::new();
    HANDLE.get_or_init(|| Arc::new(CompositorHandle::new()))
}

fn run(shared: Arc<CompositorShared>) {
    serve(&shared);
}

fn serve(shared: &Arc<CompositorShared>) {
    let mut composers: ahash::HashMap<WindowId, WindowComposer> = ahash::HashMap::default();
    loop {
        let Some(batch) = shared.take_batch() else {
            break;
        };
        let encoding_started = Instant::now();
        let inbox_latency = batch
            .iter()
            .map(|(_, delta)| delta.header.published_at)
            .min()
            .map_or(Duration::ZERO, |published| {
                encoding_started.saturating_duration_since(published)
            });
        let mut last_stats: Option<ComposeStats> = None;
        for (window, delta) in batch {
            let composer = composers.entry(window).or_insert_with(WindowComposer::new);
            match composer.compose(&delta, crate::paint::color::ColorResolver) {
                Ok(composed) => {
                    last_stats = Some(composed.stats);
                    let request = FrameRequest::from_header(delta.header, composed.payload);
                    shared.publish_frame(window, request);
                }
                Err(error) => {
                    shared.set_error(error);
                    composers.remove(&window);
                }
            }
        }
        let encode = encoding_started.elapsed();
        let retired: Vec<WindowId> = {
            let sinks = shared.lock_sinks();
            composers
                .keys()
                .copied()
                .filter(|window| sinks.get(window).is_none_or(|sink| !sink.is_alive()))
                .collect()
        };
        for window in retired {
            composers.remove(&window);
        }
        if let Some(stats) = last_stats {
            shared.record((stats, inbox_latency, encode));
        }
    }
    composers.clear();
}

pub(crate) struct CompositorTicket {
    window: WindowId,
}

impl CompositorTicket {
    pub(crate) fn new(window: WindowId) -> Self {
        Self { window }
    }
}

impl Drop for CompositorTicket {
    fn drop(&mut self) {
        compositor().unregister(self.window);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc, PoisonError,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use super::{CompositorHandle, CompositorInbox, FrameRequest, FrameSink, compositor};
    #[cfg(target_os = "linux")]
    use crate::math::LogicalSize;
    use crate::{
        math::{DevicePixelRatio, DeviceRect, dp, px},
        render::{
            compositor::{DamageRects, Plan},
            delta::{FrameHeader, SceneDelta},
        },
        window::{FrameIndex, WindowId},
    };

    fn header(index: u32) -> FrameHeader {
        FrameHeader {
            window: WindowId::from_raw(1),
            index: FrameIndex::from_raw(index),
            plan: Plan::Partial(DamageRects::new()),
            surface: (8, 8),
            scale: DevicePixelRatio::ONE,
            #[cfg(target_os = "linux")]
            surface_local: LogicalSize::new(px(8.0), px(8.0)),
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            published_at: std::time::Instant::now(),
        }
    }

    fn delta(index: u32) -> SceneDelta {
        SceneDelta::empty(header(index))
    }

    #[test]
    fn the_inbox_keeps_one_frame_per_window() {
        let inbox = CompositorInbox::new();
        let window = WindowId::from_raw(1);
        assert_eq!(inbox.publish(window, delta(1)), Some(false));
        assert_eq!(inbox.publish(window, delta(2)), Some(true), "覆盖已有帧");
        assert_eq!(inbox.absorbed(), 1);
        let batch = inbox.take_blocking().expect("有待处理帧");
        assert_eq!(batch.len(), 1, "同一窗口只占一个槽");
        assert_eq!(batch[0].0, window);
        assert_eq!(batch[0].1.header.index.get(), 2, "最新序号胜出");
        assert!(!inbox.has_pending());
    }

    #[test]
    fn the_inbox_keeps_the_newest_scroll_offset() {
        let inbox = CompositorInbox::new();
        let window = WindowId::from_raw(1);
        let scroll = |index: u32, y: f32| {
            let mut delta = delta(index);
            delta.scroll.push(crate::render::delta::ScrollDelta {
                slot: 0,
                offset: crate::math::LogicalPoint::new(px(0.0), px(y)),
                viewport: DeviceRect::new(dp(0), dp(0), dp(8), dp(8)),
            });
            delta
        };
        assert_eq!(inbox.publish(window, scroll(1, -10.0)), Some(false));
        assert_eq!(
            inbox.publish(window, scroll(2, -20.0)),
            Some(true),
            "覆盖已有帧"
        );
        assert_eq!(
            inbox.publish(window, scroll(3, -30.0)),
            Some(true),
            "再覆盖一帧"
        );
        let batch = inbox.take_blocking().expect("有待处理帧");
        assert_eq!(batch.len(), 1);
        let merged = &batch[0].1;
        assert_eq!(merged.scroll.len(), 1, "同槽只留一条");
        assert_eq!(
            merged.scroll[0].offset.y,
            px(-30.0),
            "同槽偏移必须是最新值（旧实现冻结在 −10）"
        );
    }

    #[test]
    fn different_windows_have_independent_slots() {
        let inbox = CompositorInbox::new();
        assert_eq!(inbox.publish(WindowId::from_raw(1), delta(1)), Some(false));
        assert_eq!(inbox.publish(WindowId::from_raw(2), delta(1)), Some(false));
        let batch = inbox.take_blocking().expect("两帧");
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn closing_the_inbox_wakes_the_thread() {
        let shared = Arc::new(CompositorInbox::new());
        let waiter = Arc::clone(&shared);
        let join = std::thread::spawn(move || waiter.take_blocking());
        std::thread::sleep(Duration::from_millis(10));
        shared.close();
        assert!(join.join().expect("join").is_none());
    }

    #[test]
    fn the_thread_shuts_down_and_can_be_restarted() {
        let handle = Arc::new(CompositorHandle::new());
        let sink = Arc::new(CountingSink::default());
        handle.register(WindowId::from_raw(7), sink.clone());
        assert!(handle.is_running(), "注册之后线程必须在跑");
        handle.unregister(WindowId::from_raw(7));
        assert!(!handle.is_running(), "最后一个窗口注销 ⇒ 线程已退出");
        handle.register(WindowId::from_raw(8), sink);
        assert!(handle.is_running());
        handle.shutdown();
        assert!(!handle.is_running());
    }

    #[test]
    fn the_process_compositor_is_a_singleton() {
        let first = compositor();
        let second = compositor();
        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn the_compositor_never_waits_for_a_vsync_before_encoding() {
        let handle = Arc::new(CompositorHandle::new());
        let sink = Arc::new(RecordingSink::default());
        let window = WindowId::from_raw(13);
        handle.register(window, sink.clone());
        let started = Instant::now();
        for index in 1..=30u32 {
            let mut delta = SceneDelta::empty(header(index));
            delta.header.window = window;
            delta.full = false;
            let before = sink.submitted.load(Ordering::Acquire);
            handle.publish(delta).expect("publish");
            let deadline = Instant::now() + Duration::from_secs(2);
            while sink.submitted.load(Ordering::Acquire) == before && Instant::now() < deadline {
                std::thread::sleep(Duration::from_micros(50));
            }
            assert_eq!(
                sink.submitted.load(Ordering::Acquire),
                before + 1,
                "第 {index} 笔 delta 必须被合成并交付"
            );
        }
        let elapsed = started.elapsed();
        let counters = handle.counters();
        handle.unregister(window);
        assert!(
            elapsed < Duration::from_millis(300),
            "30 笔串行 delta 的合成总时长 {elapsed:?} 必须只含编码成本；\
             若 ≈30×16.7 ms 说明合成线程又在等 vsync（旧实现 500 ms 起）"
        );
        assert_eq!(counters.frames, 30, "每笔 delta 都必须产生一帧");
        assert!(
            counters.inbox_latency < Duration::from_millis(20),
            "入队→开始编码只剩调度延迟：{:?}",
            counters.inbox_latency
        );
    }

    #[test]
    fn a_dead_compositor_thread_is_restarted_by_supervise() {
        let handle = Arc::new(CompositorHandle::new());
        let sink = Arc::new(RecordingSink::default());
        let window = WindowId::from_raw(11);
        handle.register(window, sink.clone());
        assert!(handle.is_running(), "注册之后线程必须在跑");

        handle.shutdown();
        let finished = std::thread::spawn(|| {});
        while !finished.is_finished() {
            std::thread::yield_now();
        }
        *handle
            .shared
            .join
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(finished);
        handle.shared.running.store(true, Ordering::Release);

        assert!(
            handle.supervise().expect("supervise"),
            "线程静默死亡 ⇒ 必须重启并返回「刚重启」"
        );
        assert!(handle.is_running(), "重启之后线程必须在跑");

        let mut delta = SceneDelta::empty(header(2));
        delta.header.window = window;
        delta.full = true;
        handle.publish(delta).expect("publish after restart");
        let deadline = Instant::now() + Duration::from_secs(2);
        while sink.submitted.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let delivered = sink.submitted.load(Ordering::Acquire);
        handle.unregister(window);
        assert!(delivered >= 1, "重启后的线程必须继续交付帧");
    }

    #[test]
    fn supervise_is_a_no_op_for_a_healthy_or_stopped_compositor() {
        let handle = Arc::new(CompositorHandle::new());
        assert!(!handle.supervise().expect("未启动"), "未启动 ⇒ 不重启");
        let window = WindowId::from_raw(12);
        handle.register(window, Arc::new(CountingSink::default()));
        assert!(!handle.supervise().expect("健康"), "健康线程 ⇒ 不重启");
        handle.shutdown();
        assert!(!handle.supervise().expect("已关闭"), "有序关闭 ⇒ 不重启");
    }

    #[test]
    fn publishing_into_a_closed_inbox_is_an_error() {
        let handle = Arc::new(CompositorHandle::new());
        handle.shared.inbox.close();
        let mut delta = SceneDelta::empty(header(1));
        delta.full = true;
        assert!(handle.publish(delta).is_err(), "关闭的邮箱不得静默接受");
        assert!(
            handle.take_error().is_some(),
            "错误必须登记到合成侧错误槽（下一帧 `App::frame` 冒泡）"
        );
    }

    #[derive(Default)]
    struct CountingSink {
        submitted: AtomicUsize,
    }

    impl FrameSink for CountingSink {
        fn submit(&self, _request: FrameRequest) -> bool {
            self.submitted.fetch_add(1, Ordering::AcqRel);
            true
        }

        fn is_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_published_delta_reaches_the_sink() {
        let handle = Arc::new(CompositorHandle::new());
        let sink = Arc::new(RecordingSink::default());
        handle.register(WindowId::from_raw(3), sink.clone());
        let mut delta = SceneDelta::empty(header(1));
        delta.header.window = WindowId::from_raw(3);
        delta.full = true;
        handle.publish(delta).expect("publish");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while sink.submitted.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        handle.unregister(WindowId::from_raw(3));
        assert!(
            sink.submitted.load(Ordering::Acquire) >= 1,
            "合成线程必须在 2 s 内交付一帧"
        );
    }

    #[derive(Default)]
    struct RecordingSink {
        submitted: AtomicUsize,
    }

    impl FrameSink for RecordingSink {
        fn submit(&self, request: FrameRequest) -> bool {
            assert_eq!(request.surface, (8, 8), "头部必须原样过线");
            self.submitted.fetch_add(1, Ordering::AcqRel);
            true
        }

        fn is_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn an_idle_compositor_delivers_nothing() {
        let handle = Arc::new(CompositorHandle::new());
        let sink = Arc::new(CountingSink::default());
        handle.register(WindowId::from_raw(4), sink.clone());
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(sink.submitted.load(Ordering::Acquire), 0, "空闲不得出帧");
        let counters = handle.counters();
        assert_eq!(counters.frames, 0);
        handle.unregister(WindowId::from_raw(4));
    }

    #[test]
    fn a_scroll_frame_reports_zero_encoded_lanes() {
        let handle = Arc::new(CompositorHandle::new());
        let sink = Arc::new(CountingSink::default());
        handle.register(WindowId::from_raw(5), sink.clone());
        let mut first = SceneDelta::empty(header(1));
        first.header.window = WindowId::from_raw(5);
        first.full = true;
        handle.publish(first).expect("publish");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while sink.submitted.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut scrolled = SceneDelta::empty(header(2));
        scrolled.header.window = WindowId::from_raw(5);
        scrolled.scroll.push(crate::render::delta::ScrollDelta {
            slot: 0,
            offset: crate::math::LogicalPoint::new(px(0.0), px(-8.0)),
            viewport: DeviceRect::new(dp(0), dp(0), dp(8), dp(8)),
        });
        handle.publish(scrolled).expect("publish");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while sink.submitted.load(Ordering::Acquire) < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        let counters = handle.counters();
        handle.unregister(WindowId::from_raw(5));
        assert!(
            sink.submitted.load(Ordering::Acquire) >= 2,
            "滚动帧必须被分发"
        );
        assert_eq!(counters.encoded_lanes, 0, "滚动帧不得编码任何 lane");
        assert!(!counters.encoded_on_compositor());
    }
}
