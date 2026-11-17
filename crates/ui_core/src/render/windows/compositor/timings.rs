//! 计时：`FrameTimings` 采样、`RenderStats` 组装、同步路径的完成等待。
//! 实现体由 `compositor.rs` 的薄委托调用（零行为变更的结构拆分）。

use std::time::Instant;

use super::{GpuRenderer, PresentTarget, on_gpu_thread};
use crate::{
    error::EngineError,
    render::{
        RenderStats,
        compositor::FramePlan,
        frame::{FrameRequest, FrameSlot},
        timer::{FrameTimings, GpuTimerSample, Section},
    },
};

/// 消费者：`GpuRenderer::finish_sync`（`compositor.rs` 的薄委托）。
/// 同步路径的收尾：等本帧完成 → 读本帧 GPU 时间 → 推进 retire（I4）。
pub(super) fn finish_sync(
    renderer: &mut GpuRenderer,
    slot: FrameSlot,
    timings: &mut FrameTimings,
) -> Result<(), EngineError> {
    // 同步语义：本帧命令必须完成，GPU 时间与随后的读回才有意义。
    renderer
        .stream
        .wait_slot_fence(slot.signal_value)
        .map_err(|error| renderer.classify(error))?;
    if let Some(sample) = renderer
        .timer
        .as_ref()
        .and_then(|timer| timer.resolve(slot.index))
    {
        timings.push(sample);
    }
    let present_started = Instant::now();
    timings.push(GpuTimerSample {
        section: Section::Present,
        cpu_ns: elapsed_ns(present_started),
        gpu_ns: 0,
    });
    renderer.stream.release_completed();
    Ok(())
}

/// 消费者：`GpuRenderer::submit_frame`（`compositor.rs` 的薄委托）。
/// 提交一份跨线程帧快照（窗口路径：**不等** GPU 完成，I2）。
///
/// 调用方（GPU 循环）已经等过 DXGI 的帧延迟 waitable（I3）、并用
/// [`GpuRenderer::set_content_size`] 把内容尺寸对齐到本帧；`slot` 是调用方已等到上一帧完成、
/// 尚未复位的 ring 槽（[`GpuRenderer::acquire_slot`] + [`GpuRenderer::wait_slot_fence`]）。
///
/// 本方法**不**读 timestamp、不推进 retire：这两件事只允许在观察到该帧 fence 完成之后做
/// （[`GpuRenderer::resolve_frame_timer`] / [`GpuRenderer::release_completed`]，§4.1 / §6.5）。
pub(super) fn submit_frame(
    renderer: &mut GpuRenderer,
    request: &FrameRequest,
    frame: &mut FramePlan,
    slot: FrameSlot,
) -> Result<(RenderStats, Vec<GpuTimerSample>), EngineError> {
    debug_assert!(
        on_gpu_thread(),
        "D3D12 window frames may only be submitted on the kytrix-d3d12 thread"
    );
    let mut timings = FrameTimings::new();
    let (stats, _) = renderer.submit_recorded(
        slot,
        request.index,
        &request.payload,
        frame,
        &mut timings,
        PresentTarget::Own,
    )?;

    let present_started = Instant::now();
    let presented = match renderer.swapchain.as_mut() {
        // §6.3：`Present1(1, dirty)` —— sync = 1 保持 vsync。
        Some(swapchain) => swapchain.present1(frame, 1, renderer.surface),
        None => Ok(()),
    };
    if let Err(error) = presented {
        return Err(renderer.classify(error));
    }
    renderer.fresh = false;
    timings.push(GpuTimerSample {
        section: Section::Present,
        cpu_ns: elapsed_ns(present_started),
        gpu_ns: 0,
    });
    Ok((stats, timings.samples().to_vec()))
}

/// 消费者：`FrameRecorder::record` / `finish_sync` / `submit_frame`。
pub(super) fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
