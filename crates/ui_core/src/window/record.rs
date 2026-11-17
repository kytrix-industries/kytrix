//! 帧记录与统计（`test-support` / `hud`）。

#[cfg(feature = "test-support")]
use super::FrameIndex;
#[cfg(any(feature = "test-support", feature = "hud"))]
use super::Window;
#[cfg(feature = "test-support")]
use crate::test_support::record::{FrameRecord, FrameStats};

// 消费者：`impl Window::record` 的薄委托（外部调用点见该方法文档）。
/// 最近一帧的调度记录（仅 `test-support`）。
#[cfg(feature = "test-support")]
pub(crate) fn record(window: &Window) -> Option<&FrameRecord> {
    window.record.as_ref()
}

// 消费者：`impl Window::record_idle` 的薄委托（外部调用点见该方法文档）。
/// 记录一次空闲帧（不提交、序号不变）。
#[cfg(feature = "test-support")]
pub(crate) fn record_idle(window: &mut Window) {
    window.record = Some(FrameRecord::idle(
        FrameIndex(window.scheduler.committed()),
        std::mem::take(&mut window.stats),
    ));
}

// 消费者：`impl Window::attach_a11y` 的薄委托（外部调用点见该方法文档）。
/// 把一笔 a11y 交付挂到**最近一帧**的记录上（`TestApp::drain` 在帧之后调用，与平台泵同序）。
#[cfg(feature = "test-support")]
pub(crate) fn attach_a11y(window: &mut Window, snapshot: crate::a11y::A11yTreeSnapshot) {
    if let Some(record) = window.record.as_mut() {
        record.a11y = Some(snapshot);
    }
}

// 消费者：`impl Window::set_frame_stats` 的薄委托（外部调用点见该方法文档）。
/// `App::frame` 在提交前写入本帧的统计（仅 `test-support`）。
#[cfg(feature = "test-support")]
pub(crate) fn set_frame_stats(window: &mut Window, stats: FrameStats) {
    window.stats = stats;
}

// 消费者：`impl Window::reset_debug_hud_timing` 的薄委托（外部调用点见该方法文档）。
/// 重置 HUD 的提交时间基准（开启/重新开启 HUD 时调用）。
#[cfg(feature = "hud")]
pub(crate) fn reset_debug_hud_timing(window: &mut Window) {
    window.render_stats = crate::render::RenderStats::default();
    window.frame_cpu = None;
    window.frame_interval = None;
    window.last_commit_at = None;
    window.frame_started_at = None;
    window.last_frame_latency = None;
}

// 消费者：`impl Window::note_debug_hud_commit` 的薄委托（外部调用点见该方法文档）。
/// 记录一次提交的时间戳（P17 HUD 的帧间隔/FPS 来源）。
#[cfg(feature = "hud")]
pub(crate) fn note_debug_hud_commit(window: &mut Window, now: std::time::Instant) {
    window.frame_interval = window
        .last_commit_at
        .map(|previous| now.saturating_duration_since(previous));
    window.last_commit_at = Some(now);
}

// 消费者：`impl Window::debug_hud_metrics` 的薄委托（外部调用点见该方法文档）。
/// 最近提交帧的 HUD 指标（P17）。
#[cfg(feature = "hud")]
pub(crate) fn debug_hud_metrics(window: &Window) -> crate::debug_hud::DebugHudMetrics {
    use crate::render::timer::Section;

    let (physical_width, physical_height) = window.physical_surface();
    let upload_time = window
        .timings
        .sample(Section::Upload)
        .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns));
    let gpu_time = window
        .timings
        .sample(Section::Gpu)
        .map(|sample| std::time::Duration::from_nanos(sample.gpu_ns));
    let present_time = window
        .timings
        .sample(Section::Present)
        .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns));
    // 窗口路径的 GPU 工作已移到 GPU 线程：主线程没有本帧的 samples，
    // 用最近完成帧的采样补齐（HUD 允许落后一帧）。
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    let (upload_time, gpu_time, present_time) = match window.renderer.as_ref() {
        Some(renderer) => (
            upload_time.or_else(|| {
                renderer
                    .timing_sample(Section::Upload)
                    .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns))
            }),
            gpu_time.or_else(|| {
                renderer
                    .timing_sample(Section::Gpu)
                    .map(|sample| std::time::Duration::from_nanos(sample.gpu_ns))
            }),
            present_time.or_else(|| {
                renderer
                    .timing_sample(Section::Present)
                    .map(|sample| std::time::Duration::from_nanos(sample.cpu_ns))
            }),
        ),
        None => (upload_time, gpu_time, present_time),
    };
    crate::debug_hud::DebugHudMetrics {
        frame_index: window.frame_index(),
        frame_interval: window.frame_interval,
        cpu_time: window.frame_cpu,
        upload_time,
        gpu_time,
        present_time,
        latency: window.last_frame_latency,
        draw_calls: window.render_stats.draw_calls,
        instances: window.render_stats.instances,
        primitives: window.scene.primitive_count(),
        physical_width,
        physical_height,
        scale: window.scale.factor(),
    }
}

// 消费者：`impl Window::force_full_repaint` 的薄委托（外部调用点见该方法文档）。
/// 强制全量重绘（oracle 的「全量」路径）：清 `REUSABLE` + 清空所有 lane 的内容。
#[cfg(feature = "test-support")]
pub(crate) fn force_full_repaint(window: &mut Window) {
    window.tree.force_repaint_all();
    window.scene.clear_frame();
}
