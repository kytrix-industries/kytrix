#[cfg(feature = "test-support")]
use super::FrameIndex;
#[cfg(any(feature = "test-support", feature = "hud"))]
use super::Window;
#[cfg(feature = "hud")]
use crate::render::frame::FrameFeedback;
#[cfg(feature = "test-support")]
use crate::test_support::record::{FrameRecord, FrameStats};

#[cfg(feature = "test-support")]
pub(crate) fn record(window: &Window) -> Option<&FrameRecord> {
    window.record.as_ref()
}

#[cfg(feature = "test-support")]
pub(crate) fn record_idle(window: &mut Window) {
    let mut record = FrameRecord::idle(
        FrameIndex(window.scheduler.committed()),
        std::mem::take(&mut window.stats),
    );
    record.skipped_by_budget = window.frame_budget_deferrals();
    window.record = Some(record);
}

#[cfg(feature = "test-support")]
pub(crate) fn record_budget_deferred(window: &mut Window) {
    let mut record = FrameRecord::idle(
        FrameIndex(window.scheduler.committed()),
        std::mem::take(&mut window.stats),
    );
    record.skipped_by_budget = window.frame_budget_deferrals();
    window.record = Some(record);
}

#[cfg(feature = "test-support")]
pub(crate) fn a11y_only_record(window: &mut Window) -> FrameRecord {
    let frames = window.a11y.note_a11y_only_frame();
    let stats = FrameStats {
        a11y_only_frames: frames,
        ..std::mem::take(&mut window.stats)
    };
    let mut record = FrameRecord::idle(FrameIndex(window.scheduler.committed()), stats);
    record.a11y_only = true;
    record
}

#[cfg(feature = "test-support")]
pub(crate) fn attach_a11y(window: &mut Window, snapshot: crate::a11y::A11yTreeSnapshot) {
    if let Some(record) = window.record.as_mut() {
        record.a11y = Some(snapshot);
    }
}

#[cfg(feature = "test-support")]
pub(crate) fn set_frame_stats(window: &mut Window, stats: FrameStats) {
    window.stats = stats;
}

#[cfg(feature = "test-support")]
pub(crate) fn set_frame_cpu(window: &mut Window, cpu: std::time::Duration) {
    window.stats.frame_cpu = Some(cpu);
}

#[cfg(feature = "test-support")]
pub(crate) fn set_popup_frame_work(
    window: &mut Window,
    tick: crate::popup::animation::PopupAnimationTick,
) {
    window.stats.popup_opening = tick.opening;
    window.stats.popup_closing = tick.closing;
    window.stats.popup_animation_records = tick.records;
}

#[cfg(feature = "test-support")]
pub(crate) fn set_popup_settle_rounds(window: &mut Window, rounds: usize) {
    window.stats.popup_settle_rounds = window.stats.popup_settle_rounds.saturating_add(rounds);
}

#[cfg(feature = "hud")]
pub(crate) fn reset_debug_hud_timing(window: &mut Window) {
    window.render_stats = crate::render::RenderStats::default();
    window.completed_frames = 0;
    window.frame_cpu = None;
    window.frame_started_at = None;
    window.last_frame_latency = None;
}

#[cfg(feature = "hud")]
pub(crate) fn note_completed_frame(window: &mut Window, feedback: &FrameFeedback) {
    window.last_frame_latency = Some(feedback.latency);
    window.render_stats = feedback.stats;
    window.completed_frames = window.completed_frames.saturating_add(1);
}

#[cfg(feature = "hud")]
pub(crate) fn debug_hud_metrics(window: &Window) -> crate::debug_hud::DebugHudMetrics {
    use crate::render::timer::Section;

    let (physical_width, physical_height) = window.surface;
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
        completed_frames: window.completed_frames,
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

#[cfg(feature = "test-support")]
pub(crate) fn force_full_repaint(window: &mut Window) {
    window.tree.force_repaint_all();
    window.scene.clear_frame();
}

#[cfg(all(test, feature = "hud"))]
mod tests {
    use std::time::{Duration, Instant};

    use super::{debug_hud_metrics, note_completed_frame, reset_debug_hud_timing};
    use crate::{
        render::{RenderStats, frame::FrameFeedback},
        window::{FrameIndex, Window, WindowId, WindowOptions},
    };

    fn feedback(draw_calls: u32, instances: u32, latency: Duration) -> FrameFeedback {
        FrameFeedback {
            index: FrameIndex::from_raw(1),
            stats: RenderStats {
                draw_calls,
                instances,
                geometry_uploaded_bytes: 0,
            },
            samples: Vec::new(),
            completed_at: Instant::now(),
            presented_at: None,
            latency,
        }
    }

    fn window() -> Window {
        Window::new(WindowId::from_raw(1), WindowOptions::default())
    }

    #[test]
    fn a_completed_frame_lands_the_real_draw_and_instance_counts() {
        let mut window = window();
        let metrics = debug_hud_metrics(&window);
        assert_eq!(
            (
                metrics.draw_calls,
                metrics.instances,
                metrics.completed_frames
            ),
            (0, 0, 0),
            "没有完成帧时三项都是 0"
        );

        note_completed_frame(&mut window, &feedback(7, 9, Duration::from_millis(3)));
        let metrics = debug_hud_metrics(&window);
        assert_eq!(metrics.draw_calls, 7);
        assert_eq!(metrics.instances, 9);
        assert_eq!(metrics.completed_frames, 1);
        assert_eq!(metrics.latency, Some(Duration::from_millis(3)));

        note_completed_frame(&mut window, &feedback(3, 4, Duration::from_millis(5)));
        let metrics = debug_hud_metrics(&window);
        assert_eq!((metrics.draw_calls, metrics.instances), (3, 4));
        assert_eq!(metrics.completed_frames, 2);
        assert_eq!(metrics.latency, Some(Duration::from_millis(5)));
    }

    #[test]
    fn resetting_the_hud_timing_restarts_the_observation_window() {
        let mut window = window();
        note_completed_frame(&mut window, &feedback(7, 9, Duration::from_millis(3)));
        reset_debug_hud_timing(&mut window);
        let metrics = debug_hud_metrics(&window);
        assert_eq!(
            (
                metrics.draw_calls,
                metrics.instances,
                metrics.completed_frames,
                metrics.latency
            ),
            (0, 0, 0, None)
        );
    }
}
