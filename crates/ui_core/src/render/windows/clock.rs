//! Windows 合成线程的帧时钟（B6-2）：`DCompositionWaitForCompositorClock` + DWM 合成时序。
//!
//! 共享层（`render/frame_clock.rs`）定义节奏语义与 [`FramePacing`]；本模块只提供**时钟源**：
//!
//! - 刷新率 / 相位：`platform/windows/frame_clock.rs` 的 `ClockSample`（`DwmGetCompositionTimingInfo`
//!   的 `qpcVBlank` + `qpcRefreshPeriod`，DXGI 兜底，最后 60 Hz）；
//! - 等待：`DCompositionWaitForCompositorClock`（与 DWM 的合成节拍对齐），分片等待以便关闭路径
//!   能在一个分片内打断（[`ClockWait`](../../frame_clock/struct.ClockWait.html)）。
//!
//! 目标呈现时刻 = 本次采样的 `now + until_next_present`：DWM 的 `qpcVBlank` 相位天然给出下一次
//! 可呈现的边界，不需要自己累加周期（累加会在 1/2 周期抖动上漂移）。
//!
//! **活性（不可折叠的两种结局）**：等待只有两种结局 —— 被关闭信号打断，或「这一拍没等到」。
//! 后者在显示关闭 / DWM 停摆 / 会话被调度走时是**正常**的：它只意味着本帧失去 vsync 对齐，
//! 必须继续出帧（[`FramePacing`] 会把无法对准的 tick 判成「立即呈现」）。把两者折叠成同一个
//! 布尔值交给调用方，等于把一次调度抖动当成关闭 —— 合成线程会永久退出，屏幕停在最后一帧，
//! 而输入 / 窗口外壳仍在工作（用户可见的「画面冻结」）。

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    platform::windows::frame_clock::{ClockSample, FrameClock as PlatformFrameClock},
    render::frame_clock::{
        ClockWait, FALLBACK_REFRESH_HZ, FrameClock, FrameTick, PRESENT_MARGIN, frame_interval,
        normalize_refresh_hz,
    },
};

/// 分片等待的上限：关闭路径最多等这么久就能打断一次合成器节拍等待。
const WAIT_SLICE: Duration = Duration::from_millis(4);

/// 合成节拍的来源（生产：DWM 节拍 + DXGI 兜底；单测：脚本化，用来钉死「错过一拍 ≠ 关闭」）。
trait BeatSource: Send {
    fn sample(&mut self) -> ClockSample;
    fn wait_for_beat(&mut self, timeout: Duration) -> bool;
}

impl BeatSource for PlatformFrameClock {
    fn sample(&mut self) -> ClockSample {
        PlatformFrameClock::sample(self)
    }

    fn wait_for_beat(&mut self, timeout: Duration) -> bool {
        PlatformFrameClock::wait_next_frame(self, timeout)
    }
}

/// 合成线程的帧时钟（进程级 1 个）。
pub(crate) struct CompositorClock {
    source: Box<dyn BeatSource>,
    wait: Arc<ClockWait>,
    refresh_hz: f32,
    /// 最近一次给出（或将给出）的目标呈现时刻。
    present: Instant,
}

impl CompositorClock {
    pub(crate) fn new() -> Self {
        // 桌面级时钟：`HWND::default()` 让 DXGI 兜底按主显示器定位（合成线程不属于任何窗口）。
        Self::with_source(Box::new(PlatformFrameClock::new(
            windows::Win32::Foundation::HWND::default(),
        )))
    }

    fn with_source(source: Box<dyn BeatSource>) -> Self {
        let mut clock = Self {
            source,
            wait: Arc::new(ClockWait::new()),
            refresh_hz: normalize_refresh_hz(FALLBACK_REFRESH_HZ),
            present: Instant::now(),
        };
        let sample = clock.sample();
        clock.present = Instant::now() + sample.until_next_present;
        clock
    }

    /// 一次采样（`sample()` 同时刷新缓存的刷新率，显示器热插拔 / DRR 之后自动跟上）。
    fn sample(&mut self) -> ClockSample {
        let sample = self.source.sample();
        self.refresh_hz = normalize_refresh_hz(sample.refresh_hz);
        sample
    }

    /// 等到下一个合成器节拍；**返回 `true` 只表示被关闭信号打断**。
    ///
    /// 一个刷新周期内没有节拍（显示关闭 / DWM 停摆 / 会话被调度走）返回 `false`：那是**错过
    /// 一拍**，不是关闭。这里不能无限等（帧节奏由调用方兜底），也不能把超时当成取消。
    fn wait_for_a_beat(&mut self) -> bool {
        let deadline = Instant::now() + frame_interval(self.refresh_hz) + PRESENT_MARGIN;
        loop {
            if self.wait.is_cancelled() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if self.source.wait_for_beat(WAIT_SLICE) {
                return false;
            }
        }
    }
}

// P2-5：`CompositorClock` 的 `Send` 是**自动推导**的 —— 平台时钟（`platform/windows/frame_clock.rs`
// 的 `FrameClock`）把 `HWND` 存成 `isize`，所以这里没有裸指针字段。旧实现在这里写
// `unsafe impl Send for CompositorClock {}` 来掩盖 `HWND` 的 `!Send`；不再需要。

/// 合成线程的时钟（平台入口：三平台签名一致，差异只在本模块内部）。
pub(crate) fn compositor_clock() -> Box<dyn FrameClock> {
    Box::new(CompositorClock::new())
}

impl FrameClock for CompositorClock {
    fn wait_next_frame(&mut self) -> FrameTick {
        let cancelled = self.wait_for_a_beat();
        let sample = self.sample();
        let now = Instant::now();
        // 目标呈现时刻 = 采样给出的「距下一次可呈现」；采样失败时按一帧保守估计。
        let until = sample
            .until_next_present
            .min(frame_interval(self.refresh_hz) + PRESENT_MARGIN);
        self.present = now + until.max(PRESENT_MARGIN / 2);
        FrameTick {
            now,
            present_time: self.present,
            refresh_hz: self.refresh_hz,
            cancelled,
        }
    }

    fn signal_handle(&self) -> Arc<dyn Fn() + Send + Sync> {
        let wait = Arc::clone(&self.wait);
        Arc::new(move || wait.cancel())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BeatSource, CompositorClock, WAIT_SLICE};
    use crate::{
        platform::windows::frame_clock::ClockSample,
        render::frame_clock::{FrameClock, PRESENT_MARGIN, frame_interval},
    };

    /// **P2-5**：合成线程时钟必须**自动** `Send`（`FrameClock: Send` 的移交约束），
    /// 且不再有 `unsafe impl Send`。旧实现靠 unsafe 掩盖 `HWND` 字段的 `!Send`。
    #[test]
    fn the_compositor_clock_is_send_without_an_unsafe_impl() {
        fn assert_send<T: Send>() {}
        assert_send::<CompositorClock>();
        // 合成线程的真实用法：线程内构造、线程内独占使用（返回值也是 `Send`）。
        let handle = std::thread::spawn(|| {
            let clock = CompositorClock::new();
            clock.signal_handle()();
            let mut clock = clock;
            clock.wait_next_frame().cancelled
        });
        let cancelled = handle.join().expect("compositor clock thread");
        assert!(cancelled, "取消之后必须显式标记为被打断");
    }

    /// 真实合成器时钟的冒烟测试：本机有 DWM ⇒ 采样给出合法刷新率与目标时刻；
    /// 没有 DWM（远程会话 / 无显示）⇒ 兜底 60 Hz，但**不允许** panic 或给出过去时刻。
    #[test]
    fn the_compositor_clock_gives_a_future_present_time() {
        let mut clock = CompositorClock::new();
        let tick = clock.wait_next_frame();
        assert!(tick.present_time >= tick.now, "目标呈现时刻不能在过去");
        assert!(tick.refresh_hz >= 24.0 && tick.refresh_hz <= 360.0);
        assert!(!tick.cancelled, "正常的一拍不得被标记成取消");
        // 目标时刻最多比一帧晚（采样异常时必须夹到一帧内，否则出帧会被无限推迟）。
        assert!(
            tick.present_time - tick.now <= frame_interval(tick.refresh_hz) + PRESENT_MARGIN,
            "目标呈现时刻必须夹在一帧之内"
        );
    }

    /// 关闭路径：取消之后必须在一个分片内返回（不能等满一个 vsync）。
    #[test]
    fn a_cancelled_compositor_clock_returns_quickly() {
        let mut clock = CompositorClock::new();
        clock.signal_handle()();
        let started = std::time::Instant::now();
        let tick = clock.wait_next_frame();
        assert!(tick.cancelled, "被打断的等待必须显式标记");
        assert!(started.elapsed() < WAIT_SLICE * 4, "取消之后最多等一个分片");
    }

    /// **本 bug 的回归防线**：一个刷新周期内没有合成器节拍（显示关闭 / DWM 停摆 / 会话被
    /// 调度走）是**错过一拍**，不是关闭。
    ///
    /// 旧实现让 `wait_for_a_beat` 在截止时刻返回 `false`，而 `FrameTick.completed == false` 被
    /// 合成线程当成「关闭」⇒ 合成线程永久退出、画面冻结（输入与窗口外壳仍在工作）。这里用
    /// 脚本化节拍源把「一直等不到节拍」变成确定性场景：tick 必须返回**未取消**，且必须在
    /// 截止时刻（一帧 + 提交余量）之后满载返回，不能忙等、也不能无限等。
    #[test]
    fn a_missed_beat_is_not_a_cancellation() {
        struct MissedBeats;

        impl BeatSource for MissedBeats {
            fn sample(&mut self) -> ClockSample {
                ClockSample {
                    refresh_hz: 60.0,
                    until_next_present: Duration::ZERO,
                }
            }

            fn wait_for_beat(&mut self, timeout: Duration) -> bool {
                // 模拟 `DCompositionWaitForCompositorClock` 的分片等待：到点没节拍就返回。
                std::thread::sleep(timeout);
                false
            }
        }

        let mut clock = CompositorClock::with_source(Box::new(MissedBeats));
        let started = std::time::Instant::now();
        let tick = clock.wait_next_frame();
        let elapsed = started.elapsed();
        assert!(
            !tick.cancelled,
            "没有等到节拍只是「错过一拍」：必须继续出帧，绝不能当成关闭"
        );
        assert!(
            elapsed >= frame_interval(60.0),
            "错过一拍时必须等满一个帧间隔再放行（实际 {elapsed:?}）"
        );
        assert!(
            elapsed < frame_interval(60.0) + PRESENT_MARGIN + WAIT_SLICE * 2,
            "到截止时刻必须返回（不能忙等 / 无限等，实际 {elapsed:?}）"
        );
        assert!(tick.present_time >= tick.now, "兜底目标时刻不能在过去");
    }
}
