//! 帧时钟抽象 + 「何时出帧 / 何时跳帧」的**唯一**共享实现（架构决策 B3B6 §6 / B6-2）。
//!
//! 三平台各有自己的「下一拍」来源，但**节奏语义只有一份**：
//!
//! | 平台   | 时钟源                                                                 | 目标呈现时刻 |
//! |--------|------------------------------------------------------------------------|--------------|
//! | Windows| `DCompositionWaitForCompositorClock`（`platform/windows/frame_clock.rs`）| DWM 的 `qpcVBlank + n × qpcRefreshPeriod` |
//! | macOS  | `CAMetalDisplayLinkUpdate.targetPresentationTimestamp`（host time）      | display link 给的目标时刻 |
//! | Linux  | `wl_surface.frame` 回调（+ 单调时钟推算）                                | 回调时刻 + 1 个刷新周期 |
//!
//! 三种来源都归一到同一个 [`FrameTick`]（`Instant` 时基 + 刷新率），因此**「何时提交、何时跳过、
//! 提交时钉不钉目标时刻」这套判定只有 [`FramePacing`] 一份实现**：macOS wave-4 的
//! `PresentPacing`、Windows 的合成器节拍、Linux 的 frame callback 补偿都收敛到它。
//!
//! 无窗口 / 离屏 / 单测环境用 [`ManualClock`]（确定性：完全由测试推进虚拟时间，不读真实时钟）。
//!
//! ## 等待必须可打断
//!
//! 合成线程阻塞在时钟上时，关闭路径必须能**立刻**让它醒来退出（「合成线程先于所有 GPU 线程
//! 有序退出」）。因此等待内部等的是 [`ClockWait`] 的条件变量，而 [`FrameClock::signal_handle`]
//! 给出的句柄可在任意线程置 `cancelled` 并唤醒它。

use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// 刷新率下限：低于它的「刷新率」只可能是采样错误（省电面板也不低于 24 Hz）。
pub(crate) const MIN_REFRESH_HZ: f32 = 24.0;

/// 刷新率上限：360 Hz 覆盖目前所有消费级面板，超出只可能是采样错误。
pub(crate) const MAX_REFRESH_HZ: f32 = 360.0;

/// 两级探测都失败时的兜底刷新率（60 Hz：绝大多数显示器的缺省档）。
pub(crate) const FALLBACK_REFRESH_HZ: f32 = 60.0;

/// 距离目标呈现时刻不足这个量就认为**来不及提交**。
///
/// 一个 vsync 是 16.7 ms（60 Hz）/ 8.3 ms（120 Hz）；留 2 ms 给「提交 → 合成器收到」的调度。
/// 与 macOS wave-4 的 `PresentPacing::PRESENT_MARGIN_SECS` 同值同义（收敛到本模块）。
pub(crate) const PRESENT_MARGIN: Duration = Duration::from_millis(2);

/// 连续跳过本 vsync 的上限：**1**。
///
/// 跳过的唯一收益是「不提交一帧注定迟到的内容」，而迟到帧本来也只会被合成器推到下一个 vsync；
/// 用 1/3 刷新率去换这点收益是净亏（AR3 复审 §6.2 的 P1）。因此允许跳 **1** 次重新对齐；一旦
/// 跳过了，之后**保持**立即呈现直到某个 vsync 重新赶上目标时刻。
pub(crate) const MAX_CONSECUTIVE_SKIPS: u32 = 1;

/// 目标呈现时刻与「现在」之间允许的最大提前量。
///
/// 合成器正常只给下一两个 vsync（≤ 34 ms）。异常大的「目标时刻」必须按「系统没给可用时刻」
/// 处理：`presentDrawable:atTime:` 这类 API 拿到远期时刻会让这张 drawable **永远不上屏**。
const MAX_PRESENT_LEAD: Duration = Duration::from_millis(100);

/// 刷新率规范化（三平台共用；异常值一律 clamp 到可渲染区间）。
pub(crate) fn normalize_refresh_hz(hz: f32) -> f32 {
    if !hz.is_finite() || hz <= 0.0 {
        return FALLBACK_REFRESH_HZ;
    }
    hz.clamp(MIN_REFRESH_HZ, MAX_REFRESH_HZ)
}

/// 刷新率 → 一帧的间隔。
pub(crate) fn frame_interval(hz: f32) -> Duration {
    Duration::from_secs_f32(1.0 / normalize_refresh_hz(hz))
}

/// 一次「等到下一帧」的结果（三平台统一形状）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FrameTick {
    /// 被唤醒的时刻（`Instant::now()` 的采样；测试里由 [`ManualClock`] 给虚拟时刻）。
    pub now: Instant,
    /// 本帧的**目标呈现时刻**（单调时基）。动画用它推进 `dt`（`tick.present_time - last`）。
    pub present_time: Instant,
    /// 显示器刷新率（Hz，已 [`normalize_refresh_hz`]）。
    pub refresh_hz: f32,
    /// `false` = 等待被 [`FrameClock::signal_handle`] 打断（关闭路径），不是自然到点。
    pub completed: bool,
}

/// 一次呈现决定（原 macOS `PresentPlan`，收敛到共享层）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PresentDecision {
    /// 钉到目标呈现时刻提交。
    AtTime(Instant),
    /// 目标时刻不可用，或连续迟到到不值得再等：立即提交。
    Now,
    /// 跳过本 vsync（目标时刻已经不够提交，提交注定被推到下一帧）。
    Skip,
}

/// 「何时出帧 / 何时跳过」的共享状态机（三平台唯一实现）。
///
/// 输入是一次 [`FrameTick`]，输出 [`PresentDecision`]。macOS 的 `present.rs`、Windows 的合成器
/// 节拍与 Linux 的 frame callback 补偿都走这里 —— 三端的跳帧语义因此不可能再分叉。
pub(crate) struct FramePacing {
    /// 已经连续跳过本 vsync 的次数（`0..=MAX_CONSECUTIVE_SKIPS`）。
    consecutive_skips: u32,
}

impl FramePacing {
    pub(crate) const fn new() -> Self {
        Self {
            consecutive_skips: 0,
        }
    }

    /// 判定本 tick 的呈现决定。
    pub(crate) fn plan(&mut self, tick: FrameTick) -> PresentDecision {
        // 系统没给可用目标时刻（headless / 异常值 / 巨型提前量）：回到「尽快呈现」。
        let Some(target) = usable_target(tick.now, tick.present_time) else {
            self.consecutive_skips = 0;
            return PresentDecision::Now;
        };
        if tick.now + PRESENT_MARGIN <= target {
            self.consecutive_skips = 0;
            return PresentDecision::AtTime(target);
        }
        if self.consecutive_skips >= MAX_CONSECUTIVE_SKIPS {
            // 已经跳过一个 vsync：之后**保持**「尽快呈现」，直到某个 vsync 重新赶上目标时刻
            // （只有 `AtTime` 分支清零计数）。在这里清零会回到 Skip, Skip, Now 的 1/3 刷新率。
            PresentDecision::Now
        } else {
            self.consecutive_skips += 1;
            PresentDecision::Skip
        }
    }

    /// **提交之前**的二次夹紧：用录制结束后的新时刻重新判定目标是否还够提交。
    ///
    /// `None` = 目标被录制耗时吃掉了（resize 重建、首帧编译 PSO、整页图集上传都可能花掉整个
    /// 余量）：调用方必须退回「立即呈现」，而不是把一个**已经过期**的时刻交给合成器。
    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn clamp_before_present(target: Instant, now: Instant) -> Option<Instant> {
        usable_target(now, target).filter(|target| now + PRESENT_MARGIN <= *target)
    }
}

/// 目标时刻是否可用：与「现在」的提前量在 `[0, MAX_PRESENT_LEAD]` 内。
fn usable_target(now: Instant, target: Instant) -> Option<Instant> {
    match target.checked_duration_since(now) {
        // 目标已过期：不可用（按「立即呈现」处理）。
        None => None,
        Some(lead) if lead > MAX_PRESENT_LEAD => None,
        Some(_) => Some(target),
    }
}

/// 平台原生时基（host time / QPC / `wl_surface` 回调）与 [`Instant`] 之间的**唯一**换算点。
///
/// 三个平台给的时间戳都是「某个单调原生计数」（macOS 是 host time 秒、Windows 是 QPC tick、
/// Linux 是 `Instant` 自己），而共享层只认 [`Instant`]：锚点在这里一次性建立，之后
/// [`TimeAnchor::to_instant`] / [`TimeAnchor::to_native`] 是纯函数（可在任一平台单测）。
#[derive(Clone, Copy, Debug)]
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) struct TimeAnchor {
    /// 建立锚点时的 `Instant`。
    instant: Instant,
    /// 同一时刻的平台原生值（秒）。
    native: f64,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl TimeAnchor {
    /// 建立锚点（`native_seconds` 是平台在**同一时刻**读到的原生时基）。
    pub(crate) fn new(instant: Instant, native_seconds: f64) -> Option<Self> {
        if !native_seconds.is_finite() || native_seconds < 0.0 {
            return None;
        }
        Some(Self {
            instant,
            native: native_seconds,
        })
    }

    /// 原生时基（秒）→ `Instant`；非有限值 / 负数 / 换算溢出 ⇒ `None`。
    pub(crate) fn to_instant(self, native_seconds: f64) -> Option<Instant> {
        if !native_seconds.is_finite() || native_seconds < 0.0 {
            return None;
        }
        let delta = native_seconds - self.native;
        if !delta.is_finite() {
            return None;
        }
        if delta >= 0.0 {
            self.instant
                .checked_add(Duration::try_from_secs_f64(delta).ok()?)
        } else {
            self.instant
                .checked_sub(Duration::try_from_secs_f64(-delta).ok()?)
        }
    }

    /// `Instant` → 原生时基（秒）。
    pub(crate) fn to_native(self, instant: Instant) -> f64 {
        match instant.checked_duration_since(self.instant) {
            Some(ahead) => self.native + ahead.as_secs_f64(),
            None => self.native - self.instant.duration_since(instant).as_secs_f64(),
        }
    }
}

/// Linux：`wl_surface.frame` 回调之后**下一拍**的目标呈现时刻。
///
/// 回调表示「这一帧已经被合成器取走」，下一拍 = 回调时刻 + 一个刷新周期；回调迟到（上一帧提交
/// 很慢）时不能把目标时刻算成过去 ⇒ 取 `max(callback + interval, now)`。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn next_present_from_callback(
    callback_at: Instant,
    now: Instant,
    refresh_hz: f32,
) -> Instant {
    let predicted = callback_at + frame_interval(refresh_hz);
    predicted.max(now)
}

/// 两次相邻「下一拍」发布之间观测到的刷新率（纯函数；macOS / Linux 没有同步读取刷新率的接口时
/// 用它从回调节奏反推）。
///
/// 合法区间取 `(250 Hz, 15 Hz)` 的倒数：间隔 2 ms 以下或 66 ms 以上都只可能是回调抖动 / 掉帧，
/// 不能拿来改刷新率（否则一次长暂停会把 120 Hz 的时钟钉成 15 Hz）。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn estimate_refresh_hz(previous: Instant, current: Instant) -> Option<f32> {
    let delta = current.checked_duration_since(previous)?;
    let seconds = delta.as_secs_f32();
    if !(1.0 / 250.0..=1.0 / 15.0).contains(&seconds) {
        return None;
    }
    Some(normalize_refresh_hz(1.0 / seconds))
}

/// 平台回调发布「下一拍」的进程级邮箱（macOS 的 display link / Linux 的 `wl_surface.frame`）。
///
/// 为什么是进程级的：合成线程是进程内 1 条（架构决策 §1），而回调由各窗口的平台层产生；
/// 邮箱只搬「最近一次发布的时刻」，不持有任何平台句柄，因此三个平台共用同一份实现。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) struct TickInbox {
    state: Mutex<(u64, Option<Instant>)>,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl TickInbox {
    const fn new() -> Self {
        Self {
            state: Mutex::new((0, None)),
        }
    }

    /// 平台回调：发布「本帧应在 `target` 呈现」（`None` = 只知道「可以出帧」）。
    pub(crate) fn publish(&self, target: Option<Instant>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.0 = state.0.wrapping_add(1);
        state.1 = Some(target.unwrap_or_else(Instant::now));
    }

    /// 取走上次读取之后发布的最新一拍（`(序号, 目标时刻)`；没有新的一拍 ⇒ `None`）。
    pub(crate) fn take(&self, seen: u64) -> Option<(u64, Instant)> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        (state.0 != seen).then(|| (state.0, state.1.unwrap_or_else(Instant::now)))
    }

    /// 当前序号（首次进入等待时的基线）。
    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).0
    }
}

/// 进程级邮箱（平台层用 [`publish_platform_tick`] 发布，时钟在 [`SignaledIntervalClock`] 里消费）。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn tick_inbox() -> &'static TickInbox {
    static INBOX: std::sync::OnceLock<TickInbox> = std::sync::OnceLock::new();
    INBOX.get_or_init(TickInbox::new)
}

/// 平台回调入口：发布「下一拍」的目标呈现时刻（macOS display link / Linux frame callback）。
///
/// 没有合成器回调的平台（或回调还没接线的窗口）不调用它 —— [`SignaledIntervalClock`] 按
/// 刷新率兜底，帧节奏不会因此停止。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) fn publish_platform_tick(target: Option<Instant>) {
    tick_inbox().publish(target);
}

/// 由「平台回调 + 单调间隔」共同驱动的时钟（macOS / Linux 的共享实现）。
///
/// 平台每发布一拍就重新对齐相位；没有回调时按 [`IntervalClock`] 的间隔兜底。刷新率由回调节奏
/// 反推（[`estimate_refresh_hz`]），因此不需要各平台提供「读刷新率」的接口。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) struct SignaledIntervalClock {
    interval: IntervalClock,
    seen: u64,
    last_publish: Option<Instant>,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl SignaledIntervalClock {
    pub(crate) fn new(refresh_hz: f32) -> Self {
        let now = Instant::now();
        Self {
            interval: IntervalClock::new(refresh_hz, now),
            seen: tick_inbox().generation(),
            last_publish: None,
        }
    }

    /// 下一拍的目标呈现时刻。
    pub(crate) fn present_time(&self) -> Instant {
        self.interval.present_time()
    }

    /// 显示器刷新率（由回调节奏反推，兜底 60 Hz）。
    pub(crate) fn refresh_hz(&self) -> f32 {
        self.interval.refresh_hz()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl FrameClock for SignaledIntervalClock {
    fn wait_next_frame(&mut self) -> FrameTick {
        let now = Instant::now();
        if let Some((generation, target)) = tick_inbox().take(self.seen) {
            self.seen = generation;
            // 刷新率：优先从相邻两次发布的间隔反推（display link 的节奏就是屏幕节奏）。
            if let Some(previous) = self.last_publish
                && let Some(hz) = estimate_refresh_hz(previous, target)
            {
                self.interval.set_refresh_hz(hz);
            }
            self.last_publish = Some(target);
            self.interval.align(
                next_present_from_callback(target, now, self.interval.refresh_hz()),
                now,
            );
        }
        self.interval.wait_next_frame()
    }

    fn signal_handle(&self) -> Arc<dyn Fn() + Send + Sync> {
        self.interval.signal_handle()
    }
}

/// 平台时钟工厂：三平台各一份来源，语义由本模块统一。
///
/// - Windows：DWM 合成器节拍（`render/windows/clock.rs`）；
/// - macOS / Linux：平台回调 + 单调间隔（[`SignaledIntervalClock`]），回调入口分别在
///   `render/macos/clock.rs` / `render/linux/clock.rs`。
pub(crate) fn platform_clock() -> Box<dyn FrameClock> {
    #[cfg(windows)]
    {
        Box::new(crate::render::windows::clock::CompositorClock::new())
    }
    #[cfg(target_os = "macos")]
    {
        Box::new(crate::render::macos::clock::CompositorClock::new())
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(crate::render::linux::clock::CompositorClock::new())
    }
}

/// 时钟等待原语：**粘滞**的取消位 + 条件变量（关闭路径的打断只能生效一次，之后永远醒来）。
pub(crate) struct ClockWait {
    state: Mutex<bool>,
    condvar: Condvar,
}

impl ClockWait {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(false),
            condvar: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, bool> {
        // 唤醒原语的锁不保护任何用户数据：poison 之后继续用是安全的。
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 置取消位并唤醒等待者（粘滞：之后的每次等待都立刻返回）。
    pub(crate) fn cancel(&self) {
        *self.lock() = true;
        self.condvar.notify_all();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        *self.lock()
    }

    /// 等到超时或被取消；返回 `true` = 未被取消（正常到点或超时）。
    #[cfg(any(target_os = "macos", target_os = "linux", test))]
    fn wait(&self, timeout: Duration) -> bool {
        let cancelled = self
            .condvar
            .wait_timeout_while(self.lock(), timeout, |cancelled| !*cancelled)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        !*cancelled
    }
}

impl Default for ClockWait {
    fn default() -> Self {
        Self::new()
    }
}

/// 帧时钟：合成线程唯一的「现在该不该出帧」来源。
///
/// 实现必须 `Send`（时钟随合成线程在线程内构造）；[`FrameClock::signal_handle`] 给出的句柄必须
/// `Send + Sync`，关闭路径靠它打断阻塞。
pub(crate) trait FrameClock: Send {
    /// 阻塞到下一个可呈现时刻（或被 [`FrameClock::signal_handle`] 打断）。
    ///
    /// 返回值 [`FrameTick`] **同时**回答两件事：「这一拍的目标呈现时刻」（动画 dt 的基准）
    /// 与「刷新率」（帧率上限 / 量化粒度）。因此时钟不需要额外的 getter —— 三平台的差异只在
    /// 「这一拍从哪来」，不在「怎么用」。
    fn wait_next_frame(&mut self) -> FrameTick;

    /// 可打断 [`FrameClock::wait_next_frame`] 的句柄（关闭路径用）。
    fn signal_handle(&self) -> Arc<dyn Fn() + Send + Sync>;
}

/// 单调时钟 + 固定间隔的相位对齐时钟（三平台共用兜底；Linux 的主要路径）。
///
/// `anchor` 是「已知的一拍」时刻，之后每一拍 = `anchor + n × interval`。首个 tick 之前至少等
/// 一个 `interval - PRESENT_MARGIN`，因此合成线程不会在同一个 vsync 里空转两轮。
#[cfg(any(target_os = "macos", target_os = "linux", test))]
pub(crate) struct IntervalClock {
    refresh_hz: f32,
    interval: Duration,
    /// 下一拍的目标呈现时刻。
    anchor: Instant,
    /// 关闭打断用的取消位（句柄与时钟共享同一个 `Arc`）。
    wait: Arc<ClockWait>,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl IntervalClock {
    pub(crate) fn new(refresh_hz: f32, now: Instant) -> Self {
        let refresh_hz = normalize_refresh_hz(refresh_hz);
        Self {
            refresh_hz,
            interval: frame_interval(refresh_hz),
            anchor: now,
            wait: Arc::new(ClockWait::new()),
        }
    }

    /// 用平台给的「下一拍时刻」重新对齐相位（只接受**未来**的锚点，否则退化为下一周期）。
    pub(crate) fn align(&mut self, next_present: Instant, now: Instant) {
        self.anchor = if next_present > now {
            next_present
        } else {
            now + self.interval
        };
    }

    /// 观测到新的刷新率（回调节奏反推 / 平台报告）。
    pub(crate) fn set_refresh_hz(&mut self, refresh_hz: f32) {
        let refresh_hz = normalize_refresh_hz(refresh_hz);
        if (refresh_hz - self.refresh_hz).abs() <= f32::EPSILON {
            return;
        }
        self.refresh_hz = refresh_hz;
        self.interval = frame_interval(refresh_hz);
    }

    fn wait_next_frame(&mut self) -> FrameTick {
        let now = Instant::now();
        let target = if self.anchor > now {
            self.anchor
        } else {
            // 相位已过期（上一帧迟到）：对齐到 now + 一个间隔，不尝试「追赶」。
            now + self.interval
        };
        // 在目标时刻之前留出提交余量：太晚醒来本帧就要被跳过。
        let deadline = target.checked_sub(PRESENT_MARGIN).unwrap_or(target);
        let completed = match deadline.checked_duration_since(Instant::now()) {
            Some(wait) => self.wait.wait(wait),
            None => true,
        };
        self.anchor = target + self.interval;
        FrameTick {
            now: Instant::now(),
            present_time: target,
            refresh_hz: self.refresh_hz,
            completed,
        }
    }

    /// 下一拍的目标呈现时刻（未出过帧时是「上一拍 + 1 周期」）。
    pub(crate) fn present_time(&self) -> Instant {
        self.anchor
    }

    /// 显示器刷新率（Hz）。
    pub(crate) fn refresh_hz(&self) -> f32 {
        self.refresh_hz
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl FrameClock for IntervalClock {
    fn wait_next_frame(&mut self) -> FrameTick {
        IntervalClock::wait_next_frame(self)
    }

    fn signal_handle(&self) -> Arc<dyn Fn() + Send + Sync> {
        let wait = Arc::clone(&self.wait);
        Arc::new(move || wait.cancel())
    }
}

/// 确定性时钟：**完全由测试推进**，不读真实时钟、不睡眠（`ManualClock` 驱动的测试因此稳定）。
#[cfg(test)]
pub(crate) struct ManualClock {
    refresh_hz: f32,
    interval: Duration,
    /// 虚拟「现在」（每次等待推进一个周期）。
    now: Instant,
    /// 脚本化的目标时刻偏移（`None` = 本帧没有可用目标时刻）；耗尽后按 `interval` 推算。
    script: std::collections::VecDeque<Option<Duration>>,
    waits: usize,
    cancelled: bool,
}

#[cfg(test)]
impl ManualClock {
    pub(crate) fn new(refresh_hz: f32) -> Self {
        let refresh_hz = normalize_refresh_hz(refresh_hz);
        let interval = frame_interval(refresh_hz);
        Self {
            refresh_hz,
            interval,
            now: Instant::now(),
            script: std::collections::VecDeque::new(),
            waits: 0,
            cancelled: false,
        }
    }

    /// 脚本化接下来的目标时刻（相对「现在」的偏移；`None` = 无可用目标）。
    pub(crate) fn with_targets(
        mut self,
        targets: impl IntoIterator<Item = Option<Duration>>,
    ) -> Self {
        self.script = targets.into_iter().collect();
        self
    }

    /// 等待次数（「空闲不空转」的断言依据：没有帧要出时不得增加）。
    pub(crate) fn wait_count(&self) -> usize {
        self.waits
    }

    /// 取消（关闭路径）。
    pub(crate) fn cancel(&mut self) {
        self.cancelled = true;
    }
}

#[cfg(test)]
impl FrameClock for ManualClock {
    fn wait_next_frame(&mut self) -> FrameTick {
        self.waits = self.waits.saturating_add(1);
        self.now += self.interval;
        let present_time = match self.script.pop_front() {
            Some(Some(lead)) => self.now + lead,
            // `None` = 本帧没有可用的目标时刻（系统没给 / 已过期）：按「立即呈现」判定。
            Some(None) => self
                .now
                .checked_sub(Duration::from_millis(5))
                .unwrap_or(self.now),
            None => self.now + self.interval,
        };
        FrameTick {
            now: self.now,
            present_time,
            refresh_hz: self.refresh_hz,
            completed: !self.cancelled,
        }
    }

    fn signal_handle(&self) -> Arc<dyn Fn() + Send + Sync> {
        // `ManualClock` 是同步的：没有第 2 条线程会阻塞在它上面，句柄是空操作。
        Arc::new(|| {})
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        FALLBACK_REFRESH_HZ, FrameClock, FramePacing, FrameTick, IntervalClock,
        MAX_CONSECUTIVE_SKIPS, MAX_REFRESH_HZ, MIN_REFRESH_HZ, ManualClock, PRESENT_MARGIN,
        PresentDecision, SignaledIntervalClock, TimeAnchor, estimate_refresh_hz, frame_interval,
        next_present_from_callback, normalize_refresh_hz, publish_platform_tick,
    };

    const VSYNC_60: Duration = Duration::from_micros(16_666);

    #[test]
    fn refresh_rates_are_normalized() {
        assert_eq!(normalize_refresh_hz(60.0), 60.0);
        assert_eq!(normalize_refresh_hz(144.0), 144.0);
        assert_eq!(normalize_refresh_hz(0.0), FALLBACK_REFRESH_HZ);
        assert_eq!(normalize_refresh_hz(f32::NAN), FALLBACK_REFRESH_HZ);
        assert_eq!(normalize_refresh_hz(-1.0), FALLBACK_REFRESH_HZ);
        assert_eq!(normalize_refresh_hz(1e9), MAX_REFRESH_HZ);
        assert_eq!(normalize_refresh_hz(1.0), MIN_REFRESH_HZ);
    }

    #[test]
    fn frame_interval_matches_the_refresh_rate() {
        assert_eq!(frame_interval(60.0), Duration::from_secs_f32(1.0 / 60.0));
        assert_eq!(frame_interval(120.0), Duration::from_secs_f32(1.0 / 120.0));
    }

    #[test]
    fn a_fresh_target_is_pinned() {
        let mut pacing = FramePacing::new();
        let now = Instant::now();
        let target = now + VSYNC_60;
        assert_eq!(
            pacing.plan(FrameTick {
                now,
                present_time: target,
                refresh_hz: 60.0,
                completed: true,
            }),
            PresentDecision::AtTime(target)
        );
    }

    /// 连续迟到：**最多**跳 1 个 vsync，之后保持立即呈现（不允许 1/3 刷新率）。
    #[test]
    fn a_late_target_skips_once_then_presents_immediately() {
        let mut pacing = FramePacing::new();
        let now = Instant::now();
        let late = FrameTick {
            now,
            present_time: now + PRESENT_MARGIN / 2,
            refresh_hz: 120.0,
            completed: true,
        };
        assert_eq!(pacing.plan(late), PresentDecision::Skip);
        for _ in 0..5 {
            assert_eq!(
                pacing.plan(late),
                PresentDecision::Now,
                "跳过之后必须保持提交（不能每隔几个 vsync 再跳一次）"
            );
        }
        assert_eq!(pacing.consecutive_skips, MAX_CONSECUTIVE_SKIPS);
        let caught_up = FrameTick {
            now,
            present_time: now + VSYNC_60,
            refresh_hz: 120.0,
            completed: true,
        };
        assert_eq!(
            pacing.plan(caught_up),
            PresentDecision::AtTime(now + VSYNC_60)
        );
        assert_eq!(pacing.consecutive_skips, 0);
    }

    #[test]
    fn an_expired_or_absurd_target_presents_immediately() {
        let mut pacing = FramePacing::new();
        let now = Instant::now();
        // 已经过期（目标在过去）：不可用 ⇒ 立即呈现。
        assert_eq!(
            pacing.plan(FrameTick {
                now,
                present_time: now - Duration::from_millis(5),
                refresh_hz: 60.0,
                completed: true,
            }),
            PresentDecision::Now
        );
        // 目标正好等于「现在」：还差一个提交余量 ⇒ 先跳一次，之后保持立即呈现。
        let mut pacing = FramePacing::new();
        assert_eq!(
            pacing.plan(FrameTick {
                now,
                present_time: now,
                refresh_hz: 60.0,
                completed: true,
            }),
            PresentDecision::Skip
        );
        assert_eq!(
            pacing.plan(FrameTick {
                now,
                present_time: now,
                refresh_hz: 60.0,
                completed: true,
            }),
            PresentDecision::Now
        );
        let mut pacing = FramePacing::new();
        assert_eq!(
            pacing.plan(FrameTick {
                now,
                present_time: now + Duration::from_secs(1),
                refresh_hz: 60.0,
                completed: true,
            }),
            PresentDecision::Now,
            "提前量荒谬的目标时刻按不可用处理（远期 atTime 会让 drawable 永不上屏）"
        );
    }

    #[test]
    fn clamping_before_present_uses_the_same_margin() {
        let now = Instant::now();
        let target = now + Duration::from_millis(10);
        assert_eq!(
            FramePacing::clamp_before_present(target, now + Duration::from_millis(5)),
            Some(target)
        );
        assert_eq!(
            FramePacing::clamp_before_present(target, target - PRESENT_MARGIN / 2),
            None
        );
        assert_eq!(
            FramePacing::clamp_before_present(target, target + Duration::from_millis(1)),
            None
        );
    }

    /// 空闲不空转：`ManualClock` 完全由测试推进，等待次数可断言。
    #[test]
    fn the_manual_clock_is_deterministic_and_counts_waits() {
        let mut clock = ManualClock::new(60.0);
        assert_eq!(clock.wait_count(), 0);
        let first = clock.wait_next_frame();
        assert_eq!(clock.wait_count(), 1);
        assert!(first.completed);
        assert_eq!(first.refresh_hz, 60.0);
        let second = clock.wait_next_frame();
        assert_eq!(
            second.present_time - first.present_time,
            frame_interval(60.0),
            "每次等待推进恰好一个刷新周期"
        );
        assert_eq!(clock.wait_count(), 2, "空闲时不再等待 ⇒ 计数不涨");
    }

    /// `ManualClock` 可以脚本化目标时刻（无目标 / 迟到）。
    #[test]
    fn the_manual_clock_scripts_target_times() {
        let mut clock =
            ManualClock::new(120.0).with_targets([None, Some(Duration::from_millis(16))]);
        let mut pacing = FramePacing::new();
        let missed = clock.wait_next_frame();
        assert_eq!(
            pacing.plan(missed),
            PresentDecision::Now,
            "无目标时刻 ⇒ 立即呈现"
        );
        let fresh = clock.wait_next_frame();
        assert_eq!(
            pacing.plan(fresh),
            PresentDecision::AtTime(fresh.present_time),
            "16 ms 提前量在 120 Hz 下够提交"
        );
    }

    /// 关闭路径同样作用于间隔时钟。
    #[test]
    fn a_cancelled_interval_clock_returns_immediately() {
        let mut clock = IntervalClock::new(24.0, Instant::now());
        clock.signal_handle()();
        let started = Instant::now();
        let tick = clock.wait_next_frame();
        assert!(!tick.completed);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "取消之后不得等到目标时刻"
        );
    }

    #[test]
    fn an_interval_clock_advances_by_one_interval_per_wait() {
        let mut clock = IntervalClock::new(60.0, Instant::now());
        let first = clock.wait_next_frame();
        let second = clock.wait_next_frame();
        assert!(
            second.present_time
                >= first.present_time + frame_interval(60.0) - Duration::from_millis(1),
            "两拍之间至少一个刷新周期"
        );
        assert_eq!(clock.refresh_hz(), 60.0);
    }

    // ── 平台时基换算（纯函数：三平台共用同一套判据）──────────────────────

    #[test]
    fn a_time_anchor_round_trips_between_native_and_instant() {
        let instant = Instant::now();
        let anchor = TimeAnchor::new(instant, 1_000.25).expect("合法锚点");
        assert_eq!(anchor.to_instant(1_000.25), Some(instant));
        assert_eq!(
            anchor.to_instant(1_000.75),
            instant.checked_add(Duration::from_millis(500))
        );
        assert_eq!(
            anchor.to_instant(999.75),
            instant.checked_sub(Duration::from_millis(500))
        );
        assert!((anchor.to_native(instant) - 1_000.25).abs() < 1e-9);
        let later = instant + Duration::from_millis(250);
        assert!((anchor.to_native(later) - 1_000.5).abs() < 1e-9);
    }

    #[test]
    fn a_time_anchor_rejects_absurd_native_times() {
        let instant = Instant::now();
        assert!(TimeAnchor::new(instant, -1.0).is_none());
        assert!(TimeAnchor::new(instant, f64::NAN).is_none());
        let anchor = TimeAnchor::new(instant, 0.0).expect("合法锚点");
        assert_eq!(anchor.to_instant(f64::NAN), None);
        assert_eq!(anchor.to_instant(f64::INFINITY), None);
        assert_eq!(anchor.to_instant(-5.0), None);
        // 巨大会溢出 `Instant`：必须给 `None` 而不是 panic。
        assert_eq!(anchor.to_instant(f64::MAX / 2.0), None);
    }

    /// Linux frame callback：下一拍 = 回调时刻 + 一个周期，且不得落在过去。
    #[test]
    fn the_next_present_after_a_frame_callback_is_never_in_the_past() {
        let callback = Instant::now();
        let predicted = next_present_from_callback(callback, callback, 60.0);
        assert_eq!(predicted, callback + frame_interval(60.0));

        let late_now = callback + Duration::from_millis(40);
        assert_eq!(
            next_present_from_callback(callback, late_now, 60.0),
            late_now,
            "回调迟到时目标时刻夹到「现在」而不是过去"
        );
    }

    /// 从相邻两次发布的间隔反推刷新率（macOS display link / Linux frame callback 的唯一来源）。
    #[test]
    fn the_refresh_rate_is_estimated_from_the_callback_cadence() {
        let first = Instant::now();
        let hz = estimate_refresh_hz(first, first + Duration::from_secs_f64(1.0 / 120.0))
            .expect("8.3 ms 的节奏必须是合法刷新率");
        assert!((hz - 120.0).abs() < 1.0, "反推 {hz} Hz 必须接近 120");
        // 抖动 / 掉帧：1 ms 与 100 ms 都只可能是噪声，不能拿来改刷新率。
        assert_eq!(
            estimate_refresh_hz(first, first + Duration::from_millis(1)),
            None
        );
        assert_eq!(
            estimate_refresh_hz(first, first + Duration::from_millis(100)),
            None
        );
        // 时间倒退：同样拒绝。
        assert_eq!(
            estimate_refresh_hz(first + Duration::from_millis(8), first),
            None
        );
    }

    /// 平台回调邮箱：发布一拍 → 时钟按「回调 + 一周期」对齐相位，并按回调节奏反推刷新率。
    ///
    /// 这条用例在**三个平台**上都跑（本机 Windows 也跑），因此 macOS / Linux 的时钟源不是
    /// 「只能在真机上才验证」的黑盒：能测的那一半（相位对齐 + 刷新率反推）已被钉住。
    ///
    /// 断言**不依赖墙钟精度**：`Condvar::wait_timeout` 在 Windows 上可 overshoot 一个定时器
    /// 周期（~15 ms），所以这里只钉「相位被对齐到回调时刻」与「刷新率被反推」。
    #[test]
    fn a_signalled_clock_aligns_to_published_ticks() {
        let mut clock = SignaledIntervalClock::new(FALLBACK_REFRESH_HZ);
        let base = Instant::now();
        publish_platform_tick(Some(base + Duration::from_millis(1)));
        let _first = clock.wait_next_frame();
        // 第二拍：8.33 ms 之后 ⇒ 反推 120 Hz。
        publish_platform_tick(Some(base + Duration::from_secs_f64(1.0 / 120.0)));
        let second = clock.wait_next_frame();
        assert!(
            (clock.refresh_hz() - 120.0).abs() < 20.0,
            "回调节奏反推的刷新率必须接近 120 Hz，实际 {}",
            clock.refresh_hz()
        );
        // 目标时刻必须来自「回调之后的一拍」，不是随便一个过去时刻。
        assert!(
            clock.present_time() >= base,
            "相位必须对齐到回调时刻（present={:?} base={base:?}）",
            clock.present_time()
        );
        assert!(second.refresh_hz >= 24.0);
    }

    /// 没有回调时按刷新率兜底（不能停帧），且 `IntervalClock` 的相位可被平台重新对齐。
    #[test]
    fn an_interval_clock_can_be_realigned_by_the_platform() {
        let now = Instant::now();
        let mut clock = IntervalClock::new(60.0, now);
        let aligned = now + Duration::from_millis(5);
        clock.align(aligned, now);
        assert_eq!(clock.present_time(), aligned);
        // 换屏：刷新率变化立刻生效（周期随之变化）。
        clock.set_refresh_hz(120.0);
        assert!((clock.refresh_hz() - 120.0).abs() < f32::EPSILON);
        let tick = clock.wait_next_frame();
        assert_eq!(tick.refresh_hz, 120.0);
        // 过期的锚点：退化为「现在 + 一个周期」，不会给出过去的时刻。
        clock.align(now - Duration::from_millis(50), Instant::now());
        assert!(clock.present_time() > Instant::now());
    }

    /// `ManualClock` 的取消：等待立刻返回且标记为「未完成」。
    #[test]
    fn a_cancelled_manual_clock_reports_an_incomplete_wait() {
        let mut clock = ManualClock::new(60.0);
        assert!(clock.wait_next_frame().completed);
        clock.cancel();
        assert!(!clock.wait_next_frame().completed);
    }
}
