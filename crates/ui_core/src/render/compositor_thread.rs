//! 合成线程（进程内 **1 条**）：`SceneDelta` → 增量编码 → `FramePayload` → 各窗口 GPU 线程。
//!
//! ```text
//! UI 线程                          合成线程（kytrix-compositor）            每窗口 GPU 线程
//! ─────────────────────────────     ───────────────────────────────────    ─────────────────
//! paint → SceneDelta ──► 邮箱 ──►  阻塞在邮箱（空闲零唤醒）
//!                                   取 delta（每窗口合并，最新者胜）
//!                                   阻塞到 FrameClock 的下一拍
//!                                   FramePacing：跳过 / 钉时刻 / 立即
//!                                   WindowComposer::compose（增量编码）
//!                                   FrameRequest ────────────────────►  最新帧槽 → 录制/呈现
//! ```
//!
//! ## 为什么线程是**可选**的
//!
//! 没有 GPU 线程的窗口（headless / 离屏单测）没有可分发的下游：`Window` 直接持有
//! `WindowComposer` 做 inline 合成（同一份代码，`render/compose.rs`）。线程只在**
//! 注册了 `FrameSink` 的真实窗口**时惰性启动，**最后一个窗口注销时有序退出** —— 这保证
//! 「合成线程先于所有 GPU 线程退出」（`Window` 的字段顺序：注销 ticket 在 `renderer` 之前）。
//!
//! ## 空闲不空转
//!
//! 没有 delta 时线程阻塞在邮箱的条件变量上（**不**等帧时钟），因此空闲 CPU 为 0；只有真的收到
//! delta 才进入「等下一拍」。这是与「按 vsync 空转」的关键差别（后者空闲时也有 60 次/秒唤醒）。
//!
//! ## 不静默死亡（活性）
//!
//! 主循环的退出条件只有两个：邮箱关闭（有序关闭）或**时钟显式取消**。一个刷新周期内没等到
//! 合成器节拍（显示关闭 / DWM 停摆 / 会话被调度走）只是流失一次 vsync 对齐，`FramePacing` 会
//! 把这一 tick 判成「立即呈现」—— 把它当成关闭会让合成线程永久退出，屏幕停在最后一帧而输入
//! 仍在工作（用户可见的「画面冻结」）。
//!
//! 线程仍可能因为其它原因（panic / 平台资源）静默结束：`CompositorHandle::supervise`（每帧由
//! `App::frame` 调用一次）会检测并**重启**它，同时通知调用方让所有窗口整帧重发；重启失败则
//! 以 [`EngineError::FramePipeline`] 显式失败，绝不退化成空转。

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
        frame_clock::{FrameClock, FramePacing, PresentDecision},
    },
    window::WindowId,
};

/// 合成线程的名字（诊断 / 调试器）。
pub(crate) const COMPOSITOR_THREAD_NAME: &str = "kytrix-compositor";

/// 一帧的分类计数（诊断 / HUD / 测试）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CompositorCounters {
    /// 合成线程实际合成并分发的帧数。
    pub frames: u64,
    /// 因 `FramePacing` 跳过本 vsync 的 tick 数。
    pub skipped: u64,
    /// 被合并进后继 delta 的帧数（UI 侧发布快于合成侧时增长）。
    pub absorbed: u64,
    /// 本帧重编码的 lane 数（最近一帧）。
    pub encoded_lanes: usize,
    /// 本帧复用的 lane 段数（最近一帧）。
    pub reused_segments: usize,
    /// 等待时钟的时长（最近一帧；`FrameRecord.compositor_thread_wait` 的来源）。
    pub wait: Duration,
    /// 编码时长（最近一帧；`FrameRecord.encode_on_compositor` 的来源）。
    pub encode: Duration,
}

impl CompositorCounters {
    /// 本帧是否在合成线程上做了编码（`encoded_lanes > 0`）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn encoded_on_compositor(&self) -> bool {
        self.encoded_lanes > 0
    }
}

/// 合成线程的输入邮箱：**每窗口最多一帧**（有界 + 最新者胜 + 合并）。
pub(crate) struct CompositorInbox {
    state: Mutex<InboxState>,
    condvar: Condvar,
}

struct InboxState {
    pending: Vec<(WindowId, SceneDelta)>,
    /// 被合并掉的帧数（诊断）。
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

    /// 发布一帧（同一窗口已有未取走的帧时合并：旧 → 新）。
    ///
    /// `None` = 邮箱已关闭（帧被丢弃，调用方必须把它当**帧管线已死**显式失败，不能静默）；
    /// `Some(covered)` = 已接收，`covered` 表示覆盖了同窗口一帧未取走的旧 delta。
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

    /// 阻塞到有帧可取或关闭（`None` = 关闭）：**空闲时线程停在这里，不唤醒、不空转**。
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

    /// 是否有待处理的帧（测试 / 诊断）。
    #[cfg(test)]
    pub(crate) fn has_pending(&self) -> bool {
        !self.lock().pending.is_empty()
    }

    /// 被合并掉的帧数。
    #[cfg(test)]
    pub(crate) fn absorbed(&self) -> u64 {
        self.lock().absorbed
    }

    /// 关闭邮箱（唤醒等待者，`take_blocking` 返回 `None`）。
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.condvar.notify_all();
    }

    /// 重新打开（窗口全部关闭之后又新建窗口：线程重启时复用同一个邮箱）。
    pub(crate) fn reopen(&self) {
        self.lock().closed = false;
    }

    /// 丢弃未消费的 delta（线程**意外**退出后重启：新线程没有 lane 历史，旧 delta 只会
    /// 拼出坏帧；调用方随后会要求整帧重发）。
    fn clear(&self) {
        self.lock().pending.clear();
    }
}

/// 合成线程的共享状态（句柄与线程各持一半）。
struct CompositorShared {
    inbox: CompositorInbox,
    /// 每窗口的 GPU 线程交付端点。
    sinks: Mutex<ahash::HashMap<WindowId, Arc<dyn FrameSink>>>,
    /// 帧时钟的打断句柄（线程启动时写入；关闭路径用它唤醒时钟等待）。
    cancel: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// 线程句柄（最后一个窗口注销时 join）。
    join: Mutex<Option<JoinHandle<()>>>,
    counters: Mutex<CompositorCounters>,
    /// 合成侧的内容错误（clip 链超限等）：由 UI 线程在下一个 `App::frame` 里取走并冒泡。
    error: Mutex<Option<EngineError>>,
    /// 线程是否在运行（防止重复 spawn）。
    running: AtomicBool,
}

impl CompositorShared {
    fn lock_sinks(&self) -> MutexGuard<'_, ahash::HashMap<WindowId, Arc<dyn FrameSink>>> {
        self.sinks.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 线程循环一侧（`&self`，非 `Sync` 状态全部经 `Mutex`）。
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

    fn record(&self, frame: Option<(ComposeStats, Duration, Duration)>) {
        let mut counters = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
        match frame {
            Some((stats, wait, encode)) => {
                counters.frames = counters.frames.saturating_add(1);
                counters.encoded_lanes = stats.encoded_lanes;
                counters.reused_segments = stats.reused_segments;
                counters.wait = wait;
                counters.encode = encode;
            }
            None => counters.skipped = counters.skipped.saturating_add(1),
        }
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

/// 合成线程的句柄（进程级单例；`Send + Sync`，可被任意线程持有）。
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
                cancel: Mutex::new(None),
                join: Mutex::new(None),
                counters: Mutex::new(CompositorCounters::default()),
                error: Mutex::new(None),
                running: AtomicBool::new(false),
            }),
        }
    }

    /// 注册一个窗口的交付端点（首个注册者负责启动线程）。
    pub(crate) fn register(self: &Arc<Self>, window: WindowId, sink: Arc<dyn FrameSink>) {
        self.shared.lock_sinks().insert(window, sink);
        self.ensure_running();
    }

    /// 注销窗口；**最后一个**窗口注销时把线程 join 掉（保证先于所有 GPU 线程退出）。
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

    /// 发布一条 delta（UI 线程；不阻塞）。
    ///
    /// `Err` = 邮箱已关闭（合成线程已死且重启失败）：帧被丢弃。错误同时登记到合成侧错误槽，
    /// 由 `App::frame` 的下一帧入口粘性冒泡 —— 帧管线绝不允许静默停摆。
    pub(crate) fn publish(&self, delta: SceneDelta) -> Result<(), EngineError> {
        // 窗口身份是 delta 头部的一部分（不另传参数：两个来源迟早会漂移）。
        let window = delta.header.window;
        // 覆盖（合并）计数由邮箱自己维护；这里不需要额外动作。
        if self.shared.inbox.publish(window, delta).is_some() {
            return Ok(());
        }
        let detail = SmolStr::new_static("compositor inbox closed while a window is registered");
        self.shared.set_error(EngineError::FramePipeline {
            detail: detail.clone(),
        });
        Err(EngineError::FramePipeline { detail })
    }

    /// 合成侧的内容错误（`App::frame` 每帧取一次）。
    pub(crate) fn take_error(&self) -> Option<EngineError> {
        self.shared.take_error()
    }

    /// 最近一帧的合成计数（HUD / `FrameStats` 的合成观测字段）。
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn counters(&self) -> CompositorCounters {
        *self
            .shared
            .counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 是否在运行（诊断 / 测试）。
    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    /// 监督合成线程：线程**意外**退出（不是 shutdown）时重启它。
    ///
    /// 帧管线是单点：合成线程一旦静默退出，UI 照常发布、GPU 线程空等、屏幕停在最后一帧，
    /// 用户看到的就是「画面冻结而输入仍工作」。这里把「静默死亡」变成可恢复事件：
    ///
    /// - 返回 `Ok(true)` = 刚刚重启，调用方**必须**让所有窗口整帧重发（新线程没有 lane 历史，
    ///   增量载荷会画成退化实例）；
    /// - 返回 `Ok(false)` = 线程正常（或从未启动 / 已有序关闭）；
    /// - 返回 `Err` = 线程已死且重启失败（极端资源耗尽）：只能显式失败。
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
        // 死线程没有机会清这些：未消费的 delta 与过期的取消句柄全部作废，重建线程。
        self.shared.inbox.clear();
        self.shared.running.store(false, Ordering::Release);
        *self
            .shared
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
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
        if join.is_some() {
            // 上一轮线程还没被 join（注销/重启交叉）：先收尸。
            if let Some(handle) = join.take() {
                let _ = handle.join();
            }
        }
        self.shared.inbox.reopen();
        let shared = Arc::clone(&self.shared);
        match thread::Builder::new()
            .name(COMPOSITOR_THREAD_NAME.into())
            .spawn(move || run(shared))
        {
            Ok(handle) => *join = Some(handle),
            Err(error) => {
                // 线程起不来（极端资源耗尽）：退回 inline 合成，不阻塞 UI。
                self.shared.running.store(false, Ordering::Release);
                eprintln!("kytrix: compositor thread spawn failed: {error}");
            }
        }
    }

    /// 有序关闭：关闭邮箱 → 打断时钟等待 → join。可重复调用。
    pub(crate) fn shutdown(&self) {
        self.shared.inbox.close();
        if let Some(cancel) = self
            .shared
            .cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            cancel();
        }
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

/// 进程级合成线程句柄（惰性创建；关闭后仍可复用同一个句柄再启动）。
pub(crate) fn compositor() -> &'static Arc<CompositorHandle> {
    static HANDLE: std::sync::OnceLock<Arc<CompositorHandle>> = std::sync::OnceLock::new();
    HANDLE.get_or_init(|| Arc::new(CompositorHandle::new()))
}

/// 合成线程入口：构造时钟并把取消句柄登记给关闭路径，然后进入主循环。
fn run(shared: Arc<CompositorShared>) {
    let mut clock = crate::render::frame_clock::compositor_clock();
    // 关闭路径的时钟打断句柄必须在线程等待之前注册。
    *shared.cancel.lock().unwrap_or_else(PoisonError::into_inner) = Some(clock.signal_handle());
    serve(&shared, clock.as_mut());
}

/// 合成线程主循环（时钟可注入：单测用脚本化时钟钉死「错过一拍 ≠ 关闭」）。
///
/// 退出条件只有两个：邮箱关闭（有序关闭 / 最后一个窗口注销），或时钟报告**取消**
/// （`FrameTick::cancelled`，只来自 `signal_handle`）。一个刷新周期内没有节拍**不是**退出
/// 条件 —— 那只是流失了一次 vsync 对齐，`FramePacing` 会把这一 tick 判成「立即呈现」。
fn serve(shared: &Arc<CompositorShared>, clock: &mut dyn FrameClock) {
    let mut pacing = FramePacing::new();
    let mut composers: ahash::HashMap<WindowId, WindowComposer> = ahash::HashMap::default();
    let mut pending: Option<Vec<(WindowId, SceneDelta)>> = None;
    loop {
        // 没有活 ⇒ 阻塞在邮箱上（空闲零唤醒：**不等**帧时钟）。
        if pending.is_none() {
            match shared.take_batch() {
                Some(batch) => pending = Some(batch),
                None => break,
            }
        }
        // 有活 ⇒ 对齐到下一拍（跳帧/迟到由共享的 FramePacing 决定）。
        let wait_started = Instant::now();
        let tick = clock.wait_next_frame();
        let wait = wait_started.elapsed();
        if tick.cancelled {
            break;
        }
        match pacing.plan(tick) {
            PresentDecision::Skip => {
                shared.record(None);
                continue;
            }
            PresentDecision::AtTime(_) | PresentDecision::Now => {}
        }
        let Some(batch) = pending.take() else {
            continue;
        };
        let encoding_started = Instant::now();
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
                    // 内容错误（clip 链超限）：这一帧丢弃，错误交给 UI 线程冒泡。
                    shared.set_error(error);
                    composers.remove(&window);
                }
            }
        }
        let encode = encoding_started.elapsed();
        // 已死的 GPU 线程 / 已注销的窗口：丢掉它的合成器（下一帧重新注册时会重建）。
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
            shared.record(Some((stats, wait, encode)));
        }
    }
    // 退出前清空：延迟到这里的 delta 没有下游可交付（下次启动会整帧重建）。
    composers.clear();
    *shared.cancel.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

/// 把「注销」变成 RAII：字段声明在 `renderer` **之前**，因此先于 GPU 线程 join 执行。
///
/// 这是「合成线程必须在所有 GPU 线程之前有序退出」的落点：`Window` 的字段按声明顺序析构，
/// ticket 先 drop ⇒ 注销 + （最后一个窗口时）join 合成线程 ⇒ 之后 `renderer` 才 drop。
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
            Arc, Mutex, PoisonError,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use super::{
        CompositorHandle, CompositorInbox, CompositorShared, FrameRequest, FrameSink, compositor,
        serve,
    };
    #[cfg(target_os = "linux")]
    use crate::math::Size;
    use crate::{
        math::{PhysicalRect, Scale},
        render::{
            compositor::{DamageRects, Plan},
            delta::{FrameHeader, SceneDelta},
            frame_clock::{FrameClock, FrameTick},
        },
        window::{FrameIndex, WindowId},
    };

    fn header(index: u32) -> FrameHeader {
        FrameHeader {
            window: WindowId::from_raw(1),
            index: FrameIndex::from_raw(index),
            plan: Plan::Partial(DamageRects::new()),
            surface: (8, 8),
            scale: Scale::ONE,
            #[cfg(target_os = "linux")]
            surface_local: Size::new(8.0, 8.0),
            #[cfg(target_os = "linux")]
            buffer_scale: 1,
            published_at: std::time::Instant::now(),
        }
    }

    fn delta(index: u32) -> SceneDelta {
        SceneDelta::empty(header(index))
    }

    /// 邮箱是**有界**的：同一窗口只保留最新一帧，旧帧合并进来。
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

    /// **端到端（真实合并路径）**：同一窗口连续 publish 两帧不同滚动偏移，被覆盖帧的偏移必须
    /// 让位给最新值。复审 AR4 P1-2 的实测症状正是这里：合并后偏移冻结在**最旧**值。
    #[test]
    fn the_inbox_keeps_the_newest_scroll_offset() {
        let inbox = CompositorInbox::new();
        let window = WindowId::from_raw(1);
        let scroll = |index: u32, y: f32| {
            let mut delta = delta(index);
            delta.scroll.push(crate::render::delta::ScrollDelta {
                slot: 0,
                offset: crate::math::Point::new(0.0, y),
                viewport: PhysicalRect::new(0, 0, 8, 8),
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
            merged.scroll[0].offset.y, -30.0,
            "同槽偏移必须是最新值（旧实现冻结在 −10）"
        );
    }

    /// 多个窗口各自合并（互不覆盖）。
    #[test]
    fn different_windows_have_independent_slots() {
        let inbox = CompositorInbox::new();
        assert_eq!(inbox.publish(WindowId::from_raw(1), delta(1)), Some(false));
        assert_eq!(inbox.publish(WindowId::from_raw(2), delta(1)), Some(false));
        let batch = inbox.take_blocking().expect("两帧");
        assert_eq!(batch.len(), 2);
    }

    /// 关闭之后 `take_blocking` 立刻返回 `None`（线程可退出）。
    #[test]
    fn closing_the_inbox_wakes_the_thread() {
        let shared = Arc::new(CompositorInbox::new());
        let waiter = Arc::clone(&shared);
        let join = std::thread::spawn(move || waiter.take_blocking());
        std::thread::sleep(Duration::from_millis(10));
        shared.close();
        assert!(join.join().expect("join").is_none());
    }

    /// 关闭路径：合成线程 join 之后 `is_running` 为假，且可再次启动（窗口重建）。
    #[test]
    fn the_thread_shuts_down_and_can_be_restarted() {
        let handle = Arc::new(CompositorHandle::new());
        let sink = Arc::new(CountingSink::default());
        handle.register(WindowId::from_raw(7), sink.clone());
        assert!(handle.is_running(), "注册之后线程必须在跑");
        handle.unregister(WindowId::from_raw(7));
        assert!(!handle.is_running(), "最后一个窗口注销 ⇒ 线程已退出");
        // 再次注册：同一个句柄必须能重新起线程。
        handle.register(WindowId::from_raw(8), sink);
        assert!(handle.is_running());
        handle.shutdown();
        assert!(!handle.is_running());
    }

    /// 进程级句柄是单例（两次取到同一个地址）。
    #[test]
    fn the_process_compositor_is_a_singleton() {
        let first = compositor();
        let second = compositor();
        assert!(std::ptr::eq(first, second));
    }

    /// **本 bug 的回归防线（合成线程侧）**：一次「错过一拍」的 tick 不得让主循环退出。
    ///
    /// 脚本化时钟返回 `cancelled == false` 的 tick（目标时刻在过去 ⇒ `FramePacing` 判「立即
    /// 呈现」）。旧实现把「一个刷新周期内没等到节拍」也塞进 `completed == false`，主循环据此
    /// 直接 `break` ⇒ 本测试里的 delta 永远不会被合成，sink 一次都收不到（画面永久冻结）。
    #[test]
    fn a_missed_beat_does_not_stop_the_compositor_loop() {
        struct MissedBeats;

        impl FrameClock for MissedBeats {
            fn wait_next_frame(&mut self) -> FrameTick {
                let now = Instant::now();
                FrameTick {
                    now,
                    // 错过一拍之后的典型状态：没有可对准的目标时刻。
                    present_time: now - Duration::from_millis(1),
                    refresh_hz: 60.0,
                    cancelled: false,
                }
            }

            fn signal_handle(&self) -> Arc<dyn Fn() + Send + Sync> {
                Arc::new(|| {})
            }
        }

        let shared = Arc::new(CompositorShared {
            inbox: CompositorInbox::new(),
            sinks: Mutex::new(ahash::HashMap::default()),
            cancel: Mutex::new(None),
            join: Mutex::new(None),
            counters: Mutex::new(super::CompositorCounters::default()),
            error: Mutex::new(None),
            running: AtomicBool::new(true),
        });
        let sink = Arc::new(RecordingSink::default());
        let window = WindowId::from_raw(9);
        shared.lock_sinks().insert(window, sink.clone());
        let mut pending = SceneDelta::empty(header(1));
        pending.header.window = window;
        pending.full = true;
        assert_eq!(shared.inbox.publish(window, pending), Some(false));

        let runner = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let mut clock = MissedBeats;
                serve(&shared, &mut clock);
            })
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while sink.submitted.load(Ordering::Acquire) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let delivered = sink.submitted.load(Ordering::Acquire);
        // 有序退出：关闭邮箱让 `take_blocking` 返回 `None`。
        shared.inbox.close();
        runner.join().expect("compositor loop");
        assert!(
            delivered >= 1,
            "错过一拍必须继续合成并交付；若在这里为 0，说明主循环把「没等到节拍」当成了关闭"
        );
    }

    /// 取消（关闭路径）必须**先于**合成退出：取消的 tick 不得再合成 / 交付。
    #[test]
    fn a_cancelled_tick_stops_the_loop_before_delivering() {
        struct CancelledClock;

        impl FrameClock for CancelledClock {
            fn wait_next_frame(&mut self) -> FrameTick {
                let now = Instant::now();
                FrameTick {
                    now,
                    present_time: now + Duration::from_millis(8),
                    refresh_hz: 60.0,
                    cancelled: true,
                }
            }

            fn signal_handle(&self) -> Arc<dyn Fn() + Send + Sync> {
                Arc::new(|| {})
            }
        }

        let shared = Arc::new(CompositorShared {
            inbox: CompositorInbox::new(),
            sinks: Mutex::new(ahash::HashMap::default()),
            cancel: Mutex::new(None),
            join: Mutex::new(None),
            counters: Mutex::new(super::CompositorCounters::default()),
            error: Mutex::new(None),
            running: AtomicBool::new(true),
        });
        let sink = Arc::new(RecordingSink::default());
        let window = WindowId::from_raw(10);
        shared.lock_sinks().insert(window, sink.clone());
        let mut pending = SceneDelta::empty(header(1));
        pending.header.window = window;
        pending.full = true;
        assert_eq!(shared.inbox.publish(window, pending), Some(false));

        serve(&shared, &mut CancelledClock);
        assert_eq!(
            sink.submitted.load(Ordering::Acquire),
            0,
            "取消之后不得再交付帧（关闭路径优先）"
        );
    }

    /// **本 bug 的回归防线（句柄侧）**：合成线程**静默死亡**（不是 shutdown：`running` 仍为真、
    /// 没人关过邮箱）必须被 `supervise` 重启，并通知调用方整帧重发。
    #[test]
    fn a_dead_compositor_thread_is_restarted_by_supervise() {
        let handle = Arc::new(CompositorHandle::new());
        let sink = Arc::new(RecordingSink::default());
        let window = WindowId::from_raw(11);
        handle.register(window, sink.clone());
        assert!(handle.is_running(), "注册之后线程必须在跑");

        // 制造「running 为真、邮箱开着、线程已结束」的静默死亡现场：先有序关闭（join 真线程），
        // 再把一个**已结束但未 join** 的句柄塞回 join 槽。
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

        // 新线程能继续交付（整帧重发由 `App::frame` 负责，这里只验证活性）。
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

    /// 健康 / 未启动 / 已关闭的合成器上 `supervise` 必须是 no-op（不重启、不报错）。
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

    /// 邮箱关闭之后 `publish` 必须**显式失败**（而不是静默丢弃），并登记粘性错误。
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

    /// 一帧真实数据走完整条链路：UI 线程发布 delta → 合成线程编码 → sink 收到载荷。
    ///
    /// 用 `ManualClock` 之外的真实时钟（合成线程内部构造平台时钟）：这里只断言**时序**
    /// （一段有限时间内必有一帧到达），不断言具体时刻。
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

    /// 没有 delta 时不得产生任何交付（空闲零唤醒的反向断言）。
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

    /// 滚动帧的计数：`encoded_lanes == 0`（合成线程侧的可观测断言）。
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
        // 只有偏移的帧。
        let mut scrolled = SceneDelta::empty(header(2));
        scrolled.header.window = WindowId::from_raw(5);
        scrolled.scroll.push(crate::render::delta::ScrollDelta {
            slot: 0,
            offset: crate::math::Point::new(0.0, -8.0),
            viewport: PhysicalRect::new(0, 0, 8, 8),
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
