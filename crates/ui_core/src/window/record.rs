//! 帧记录与统计（`test-support` / `hud`）。

#[cfg(feature = "test-support")]
use super::FrameIndex;
#[cfg(any(feature = "test-support", feature = "hud"))]
use super::Window;
#[cfg(feature = "hud")]
use crate::render::frame::FrameFeedback;
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
    let mut record = FrameRecord::idle(
        FrameIndex(window.scheduler.committed()),
        std::mem::take(&mut window.stats),
    );
    record.skipped_by_budget = window.frame_budget_deferrals();
    window.record = Some(record);
}

// 消费者：`impl Window::record_budget_deferred` 的薄委托（外部调用点见该方法文档）。
/// 记录一次「被帧预算门禁推迟」的请求（A5）：空闲帧 + 累计推迟计数。
///
/// 与 [`record_idle`] 的区别只有 `skipped_by_budget` 的可断言性：它证明「帧没有丢，只是被
/// 合并到下一拍」，而不是掉帧（`FrameIndex` 不变、脏位保留）。
#[cfg(feature = "test-support")]
pub(crate) fn record_budget_deferred(window: &mut Window) {
    let mut record = FrameRecord::idle(
        FrameIndex(window.scheduler.committed()),
        std::mem::take(&mut window.stats),
    );
    record.skipped_by_budget = window.frame_budget_deferrals();
    window.record = Some(record);
}

// 消费者：`impl Window::record_a11y_only_frame` 的薄委托（外部调用点见该方法文档）。
/// a11y-only 帧（R13-6）的记录：没有像素工作、`FrameIndex` 不变，只带 a11y 指标。
///
/// 与 [`record_idle`] 的区别只有两处：`a11y_only` 旗标与窗口级累计计数
/// （`FrameStats::a11y_only_frames`），后者是"快速路径真的在跑"的断言面。
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
/// 重置 HUD 的观测基准（开启/重新开启 HUD 时调用）：旧的采样窗口与成本累计全部作废。
#[cfg(feature = "hud")]
pub(crate) fn reset_debug_hud_timing(window: &mut Window) {
    window.render_stats = crate::render::RenderStats::default();
    window.completed_frames = 0;
    window.frame_cpu = None;
    window.frame_started_at = None;
    window.last_frame_latency = None;
}

// 消费者：`poll_renderer`（三平台窗口泵每轮调用；本 crate 内唯一的调用点）。
/// 一帧**完成**的反馈：HUD 的 `Latency` / `Draw` / `Inst` 与完成帧计数都在这里落账。
///
/// threaded 路径（真实窗口）里这是 `render_stats` 的**唯一**写点：UI 线程提交帧时并不知道后端
/// 实际发了多少 draw call，真实计数随 `FrameFeedback` 回来。反馈槽是三端共用的「最新者胜」
/// 槽，因此计数是「UI 线程观测到的完成帧数」（同一泵迭代里完成多帧时只看到最后到达的一条）。
#[cfg(feature = "hud")]
pub(crate) fn note_completed_frame(window: &mut Window, feedback: &FrameFeedback) {
    window.last_frame_latency = Some(feedback.latency);
    window.render_stats = feedback.stats;
    window.completed_frames = window.completed_frames.saturating_add(1);
}

// 消费者：`impl Window::debug_hud_metrics` 的薄委托（外部调用点见该方法文档）。
/// 提交帧 + 完成帧计数的 HUD 指标（P17）。
///
/// 一次读取混合几个时刻的数据，但每一项都是**真实观测值**而不是推算：
/// - 逐帧成本（`cpu_time`）与 `primitives`：最近一次通过帧门禁的提交帧 / 当前场景；
/// - `draw_calls` / `instances` / `latency`：最近一条 `FrameFeedback`（**已完成**帧；threaded
///   路径的唯一来源，从没观测到完成帧时是默认值 0，由 HUD 显示成 `--`）；
/// - `physical_width` / `physical_height` / `scale`：当前 `window.surface` 与比例（帧入口按帧
///   刷新，尺寸行不滞后采样窗口）；
/// - `completed_frames`：GPU 线程已经**完成**的帧数（单调累计，采样窗口用它做差）。
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

// 消费者：`impl Window::force_full_repaint` 的薄委托（外部调用点见该方法文档）。
/// 强制全量重绘（oracle 的「全量」路径）：清 `REUSABLE` + 清空所有 lane 的内容。
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

    /// **P-B 的核心断言**：完成反馈把后端真实计数写进 HUD 指标（旧实现里 threaded 路径没有
    /// 任何写者 ⇒ Draw/Inst 恒 0）。
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

        // 下一帧：覆盖为本帧的真实值，计数继续累计。
        note_completed_frame(&mut window, &feedback(3, 4, Duration::from_millis(5)));
        let metrics = debug_hud_metrics(&window);
        assert_eq!((metrics.draw_calls, metrics.instances), (3, 4));
        assert_eq!(metrics.completed_frames, 2);
        assert_eq!(metrics.latency, Some(Duration::from_millis(5)));
    }

    /// 重新安装 HUD = 新的观测窗口：完成帧计数与上一次的统计一起清零。
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
