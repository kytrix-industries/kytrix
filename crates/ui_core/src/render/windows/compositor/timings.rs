//! 计时：`FrameTimings` 采样、`RenderStats` 组装、同步路径的完成等待，以及 `Present1` 的结果
//! 落地（R08-7 的重试 / 重建决策）。
//! 实现体由 `compositor.rs` 的薄委托调用。

use std::time::Instant;

use super::{GpuRenderer, on_gpu_thread};
use crate::{
    error::EngineError,
    render::{
        RenderStats,
        compositor::FramePlan,
        frame::{FrameRequest, FrameSlot},
        timer::{FrameTimings, GpuTimerSample, Section},
        windows::{diagnostics, hresult::PresentOutcome},
    },
};

/// `Present1` 失败之后要执行的动作（R08-7）。
///
/// 关键分类（见 [`super::super::hresult::classify_present`]）：
/// - `InvalidCall`：用「整面 dirty」重试一次（多半是驱动拒绝了某个矩形）；
/// - `ModeChanged`：交换链 out of date ⇒ 重建目标后重录这一帧（[`FrameSubmitError::NeedsResize`]）；
/// - 设备级错误：**唯一**进设备丢失路径的类别；
/// - 其它负 HRESULT：致命（保留分类，窗口上报错误）。
pub(crate) enum FrameSubmitError {
    Fatal(EngineError),
    /// 交换链 out of date / 显示模式变化：调用方 `resize_target` 之后重录这一帧。
    NeedsResize,
}

impl FrameSubmitError {
    fn fatal(error: EngineError) -> Self {
        Self::Fatal(error)
    }
}

/// 消费者：`GpuRenderer::finish_sync`（`compositor.rs` 的薄委托）。
/// 同步路径的收尾：等本帧完成 → 读本帧 GPU 时间 → 推进 retire（I4）。
pub(super) fn finish_sync(
    renderer: &mut GpuRenderer,
    slot: FrameSlot,
    timings: &mut FrameTimings,
) -> Result<(), EngineError> {
    // 同步语义：本帧命令必须完成，GPU 时间与随后的读回才有意义。
    if !renderer
        .stream
        .wait_slot_fence(slot.signal_value)
        .map_err(|error| renderer.classify(error))?
        .is_signaled()
    {
        // 设备健康但没等到（R08-3）：同步路径（快照 / 读回）无法继续，但这不是设备丢失。
        return Err(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static(
                "synchronous frame did not complete while the device is still present",
            ),
        });
    }
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
/// 提交一份跨线程帧快照并 present（窗口路径：**不等** GPU 完成，I2）。
///
/// 调用方（GPU 循环）已经等过 DXGI 的帧延迟 waitable（I3）、并用
/// [`GpuRenderer::set_content_size`] 把内容尺寸对齐到本帧；`slot` 是调用方已等到上一帧完成、
/// 尚未复位的 ring 槽。
///
/// `replace` = 本帧提交时已经有**更新的**帧待画（UI 又发布了一帧）：此时 `Present1(sync = 0)`
/// 让 DWM 立刻替换队列里的那帧而不是再等一个 vsync（R08-9 的方案 C ②）。稳态下 `replace`
/// 为 `false`（`sync = 1`，配合 waitable 是标准做法）。
///
/// 本方法**不**读 timestamp、不推进 retire：这两件事只允许在观察到该帧 fence 完成之后做。
pub(super) fn submit_frame(
    renderer: &mut GpuRenderer,
    request: &FrameRequest,
    frame: &mut FramePlan,
    slot: FrameSlot,
    replace: bool,
) -> Result<(RenderStats, Vec<GpuTimerSample>), FrameSubmitError> {
    debug_assert!(
        on_gpu_thread(),
        "D3D12 window frames may only be submitted on the kytrix-d3d12 thread"
    );
    let mut timings = FrameTimings::new();
    let (recorded, _) = renderer
        .submit_recorded(slot, request.index, &request.payload, frame, &mut timings)
        .map_err(FrameSubmitError::fatal)?;

    let present_started = Instant::now();
    let mut outcome = PresentOutcome::Presented;
    if let Some(swapchain) = renderer.swapchain.as_mut() {
        // 有新帧待画 ⇒ `sync = 0`（取消上一帧剩余时间）；否则 `sync = 1` 保持 vsync 节奏。
        let sync = if replace { 0 } else { 1 };
        outcome = swapchain.present1(recorded.present.as_ref(), sync);
        if outcome == PresentOutcome::InvalidCall {
            // R08-7：驱动拒绝了某个矩形（越界 / 参数），退化为整面再试一次。
            diagnostics::present_retried();
            outcome = swapchain.present1(None, sync);
        }
    }
    // R08-11 的不变量：全量帧必须整面 present（`DirtyRectsCount = 0`）。
    debug_assert!(
        !frame.is_full() || recorded.present.is_none(),
        "a full frame must present the whole surface"
    );
    match outcome {
        PresentOutcome::Presented | PresentOutcome::Clipped => {
            renderer.target.on_presented();
            // 计划里的 fresh 也跟着推进（唯一状态源在 `TargetState`，这里只是保持两者一致）。
            frame.presented();
        }
        PresentOutcome::Occluded => {
            // 组合链通常拿不到遮挡（组合链没有遮挡概念），真拿到就按「本帧未上屏」处理：
            // 下一帧整帧重来，并让 GPU 循环降频。
            diagnostics::occluded_skipped();
            renderer.target.on_present_failed();
        }
        PresentOutcome::ModeChanged => {
            renderer.target.on_present_failed();
            return Err(FrameSubmitError::NeedsResize);
        }
        PresentOutcome::DeviceLost => {
            renderer.target.on_present_failed();
            let error = EngineError::DeviceLost {
                reason: smol_str::SmolStr::new_static("Present1 reported a removed device"),
                log_path: std::path::PathBuf::new(),
            };
            return Err(FrameSubmitError::fatal(renderer.classify(error)));
        }
        PresentOutcome::InvalidCall | PresentOutcome::Failed => {
            diagnostics::present_failed();
            renderer.target.on_present_failed();
            return Err(FrameSubmitError::fatal(EngineError::Swapchain {
                detail: smol_str::format_smolstr!(
                    "Present1({} dirty rects) failed: {outcome:?}",
                    recorded
                        .present
                        .as_ref()
                        .map_or(0, |damage| damage.as_slice().len())
                ),
            }));
        }
    }
    timings.push(GpuTimerSample {
        section: Section::Present,
        cpu_ns: elapsed_ns(present_started),
        gpu_ns: 0,
    });
    Ok((recorded.stats, timings.samples().to_vec()))
}

/// 消费者：`FrameRecorder::record` / `finish_sync` / `submit_frame`。
pub(super) fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}
