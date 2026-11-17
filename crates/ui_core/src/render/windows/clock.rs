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

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    platform::windows::frame_clock::{ClockSample, FrameClock as CompositorClockSource},
    render::frame_clock::{
        ClockWait, FrameClock, FrameTick, PRESENT_MARGIN, frame_interval, normalize_refresh_hz,
    },
};

/// 分片等待的上限：关闭路径最多等这么久就能打断一次合成器节拍等待。
const WAIT_SLICE: Duration = Duration::from_millis(4);

/// 合成线程的帧时钟（进程级 1 个）。
pub(crate) struct CompositorClock {
    source: CompositorClockSource,
    wait: Arc<ClockWait>,
    refresh_hz: f32,
    /// 最近一次给出（或将给出）的目标呈现时刻。
    present: Instant,
}

impl CompositorClock {
    pub(crate) fn new() -> Self {
        // 桌面级时钟：`HWND::default()` 让 DXGI 兜底按主显示器定位（合成线程不属于任何窗口）。
        let mut source = CompositorClockSource::new(windows::Win32::Foundation::HWND::default());
        let sample = source.sample();
        Self {
            refresh_hz: normalize_refresh_hz(sample.refresh_hz),
            present: Instant::now() + sample.until_next_present,
            source,
            wait: Arc::new(ClockWait::new()),
        }
    }

    /// 一次采样（`sample()` 同时刷新缓存的刷新率，显示器热插拔 / DRR 之后自动跟上）。
    fn sample(&mut self) -> ClockSample {
        let sample = self.source.sample();
        self.refresh_hz = normalize_refresh_hz(sample.refresh_hz);
        sample
    }

    /// 等到下一个合成器节拍；`false` = 被打断，或在一个刷新周期内没有节拍
    /// （显示关闭 / DWM 停摆 / 无合成器的会话：不能在这里无限循环，帧节奏由调用方兜底）。
    fn wait_for_a_beat(&mut self) -> bool {
        let deadline = Instant::now() + frame_interval(self.refresh_hz) + PRESENT_MARGIN;
        loop {
            if self.wait.is_cancelled() {
                return false;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if self.source.wait_next_frame(WAIT_SLICE) {
                return true;
            }
        }
    }
}

// SAFETY: `CompositorClock` 唯一的非 `Send` 字段是平台时钟里的 `HWND`（一个进程范围的句柄值，
// 只用于 DXGI 兜底时定位显示器）。句柄值本身可以自由复制到另一个线程；本类型在合成线程里
// **构造并独占使用**，`Send` 只为满足 `Box<dyn FrameClock>` 的移交约束。
unsafe impl Send for CompositorClock {}

impl FrameClock for CompositorClock {
    fn wait_next_frame(&mut self) -> FrameTick {
        let completed = self.wait_for_a_beat();
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
            completed,
        }
    }

    fn signal_handle(&self) -> Arc<dyn Fn() + Send + Sync> {
        let wait = Arc::clone(&self.wait);
        Arc::new(move || wait.cancel())
    }
}

#[cfg(test)]
mod tests {
    use super::{CompositorClock, WAIT_SLICE};
    use crate::render::frame_clock::{FrameClock, PRESENT_MARGIN, frame_interval};

    /// 真实合成器时钟的冒烟测试：本机有 DWM ⇒ 采样给出合法刷新率与目标时刻；
    /// 没有 DWM（远程会话 / 无显示）⇒ 兜底 60 Hz，但**不允许** panic 或给出过去时刻。
    #[test]
    fn the_compositor_clock_gives_a_future_present_time() {
        let mut clock = CompositorClock::new();
        let tick = clock.wait_next_frame();
        assert!(tick.present_time >= tick.now, "目标呈现时刻不能在过去");
        assert!(tick.refresh_hz >= 24.0 && tick.refresh_hz <= 360.0);
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
        assert!(!tick.completed, "被打断的等待必须显式标记");
        assert!(started.elapsed() < WAIT_SLICE * 4, "取消之后最多等一个分片");
    }
}
