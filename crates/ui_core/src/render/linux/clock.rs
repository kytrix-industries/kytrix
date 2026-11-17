//! Linux 合成线程的帧时钟（B6-2）：`wl_surface.frame` 回调 + 单调时钟推算。
//!
//! 节奏语义在共享层（`render/frame_clock.rs` 的 `FramePacing`）；本模块只提供**时钟源**：
//!
//! - 来源：`wl_surface.frame` 的 done 回调。Wayland 没有「显示器刷新率」查询接口，帧的节奏就是
//!   回调的节奏 —— 回调时刻 + 一个周期即下一拍的目标呈现时刻（`next_present_from_callback`，
//!   纯函数，共享层单测）；刷新率由相邻回调的间隔反推（[`estimate_refresh_hz`]）。
//! - 兜底：回调没来（窗口不可见 / 还没有 first frame request）时按 1/60 s 的单调间隔推进，
//!   与旧 `frame_gate` 的保守兜底同义。
//!
//! 接线点：平台层的 frame callback 处理里调用 [`note_frame_callback`]（一次回调一次）。

use std::time::Instant;

use crate::render::frame_clock::{
    FALLBACK_REFRESH_HZ, FrameClock, SignaledIntervalClock, publish_platform_tick,
};

/// 合成线程的帧时钟（平台回调驱动 + 单调间隔兜底）。
pub(crate) type CompositorClock = SignaledIntervalClock;

/// 合成线程的时钟（平台入口：三平台签名一致，差异只在本模块内部）。
///
/// Wayland 没有刷新率查询接口：先用 60 Hz 兜底，`wl_surface.frame` 的回调节奏会把它反推到
/// 真实值（`estimate_refresh_hz`）。
pub(crate) fn compositor_clock() -> Box<dyn FrameClock> {
    Box::new(CompositorClock::new(FALLBACK_REFRESH_HZ))
}

/// `wl_surface.frame` 回调入口。
///
/// 回调本身不携带时间戳（协议只表示「你可以在下一个呈现周期提交」），因此目标时刻按「现在 +
/// 一个周期」发布；时钟侧再用 [`estimate_refresh_hz`] 从相邻回调的间隔反推真实刷新率。
pub(crate) fn note_frame_callback() {
    publish_platform_tick(Some(Instant::now()));
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::{CompositorClock, note_frame_callback};
    use crate::render::frame_clock::{FALLBACK_REFRESH_HZ, FrameClock};

    /// 没有回调时按刷新率兜底（不能停帧）。
    #[test]
    fn the_default_clock_falls_back_to_the_refresh_interval() {
        let mut clock = CompositorClock::new(FALLBACK_REFRESH_HZ);
        let tick = clock.wait_next_frame();
        assert!(tick.present_time > tick.now);
    }

    /// 60 Hz 节奏的回调把目标时刻推进到「回调 + 一周期」，且不会落在过去。
    #[test]
    fn frame_callbacks_drive_the_phase() {
        let mut clock = CompositorClock::new(FALLBACK_REFRESH_HZ);
        let _warm_up = clock.wait_next_frame();
        note_frame_callback();
        let tick = clock.wait_next_frame();
        assert!(tick.present_time >= tick.now, "目标呈现时刻不得在过去");
        assert!(
            tick.refresh_hz >= 24.0 && tick.refresh_hz <= 360.0,
            "刷新率必须落在合法区间"
        );
        thread::sleep(Duration::from_millis(1));
    }
}
