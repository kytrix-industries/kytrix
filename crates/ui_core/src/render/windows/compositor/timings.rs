//! 计时：`FrameTimings` 采样、`RenderStats` 组装、同步路径的完成等待，以及 `Present1` 的结果
//! 落地（R08-7 的重试 / 重建决策）。
//! 实现体由 `compositor.rs` 的薄委托调用。

use std::time::Instant;

use super::{GpuRenderer, on_gpu_thread};
use crate::{
    error::EngineError,
    render::{
        RenderStats,
        compositor::{DamageRects, FramePlan},
        frame::{FrameRequest, FrameSlot},
        timer::{FrameTimings, GpuTimerSample, Section},
        windows::{diagnostics, hresult::PresentOutcome},
    },
};

/// `Present1` 失败之后要执行的动作（R08-7）。
///
/// 关键分类（见 [`super::super::hresult::classify_present`]）：
/// - `InvalidCall`：只有**整帧重录**是安全的（见 [`invalid_call_recovery`]）
///   ⇒ [`FrameSubmitError::RetryFull`]，由 GPU 循环用 `Plan::Full` 重录这一帧；
/// - `ModeChanged`：交换链 out of date ⇒ 重建目标后重录这一帧（[`FrameSubmitError::NeedsResize`]）；
/// - 设备级错误：**唯一**进设备丢失路径的类别；
/// - 其它负 HRESULT：致命（保留分类，窗口上报错误）。
pub(crate) enum FrameSubmitError {
    Fatal(EngineError),
    /// 交换链 out of date / 显示模式变化：调用方 `resize_target` 之后重录这一帧。
    NeedsResize,
    /// `Present1` 拒绝了本帧（`DXGI_ERROR_INVALID_CALL`）：调用方把这一帧**整帧重录**
    /// （`Plan::Full` ⇒ `Direct`：整块 clear + 全内容重画 + 整面 present）后再 present。
    RetryFull,
}

impl FrameSubmitError {
    fn fatal(error: EngineError) -> Self {
        Self::Fatal(error)
    }
}

/// `Present1` 返回 `DXGI_ERROR_INVALID_CALL` 之后的动作（AR3 §4.2 缺口 1 / P2-4）。
///
/// **为什么不能「换成整面 dirty 再 present 一次」**：增量帧的当前 back buffer 只同步了
/// `pending ∪ damage` 这些矩形（`plan::BufferSyncs`），而 `Present1` 的 dirty 参数只是
/// **声明**、不会让未同步的像素变得有内容。整面 present 会把那些区域直接上屏 —— flip 模型下
/// 它们是这张 buffer 上一次呈现的内容（可能 3 帧前），`Fresh` 时甚至是未定义内容。所以：
///
/// - 增量帧（`presented = Some`）⇒ 必须**整帧重录**：`Direct` 路径整块 clear + 全内容重画 +
///   整面 present，back buffer 的每个像素都有定义（复用三模式里已有的路径，不新增模式）；
/// - 整帧（`presented = None`）⇒ back buffer 已经整块与 persist 一致（`Direct` 清屏重画 /
///   `Full` 整块拷贝），**不存在**未同步区域；此时「整面重试」就是刚才那次失败的调用本身，
///   重试没有意义 ⇒ 按致命错误上报。
pub(crate) fn invalid_call_recovery(presented: Option<&DamageRects>) -> PresentRetry {
    match presented {
        Some(_) => PresentRetry::FullRerecord,
        None => PresentRetry::Fatal,
    }
}

/// [`invalid_call_recovery`] 的返回值（纯决策，单测覆盖）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentRetry {
    /// 本帧整帧重录（`Plan::Full`）后再 present。
    FullRerecord,
    /// 没有安全的退路：按致命错误上报。
    Fatal,
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
        if outcome == PresentOutcome::InvalidCall
            && invalid_call_recovery(recorded.present.as_ref()) == PresentRetry::FullRerecord
        {
            // R08-7 / AR3 P2-4：**不**换参数重试（整面 present 会把未同步区域上屏），而是交给
            // GPU 循环整帧重录。这里已经发生的状态推进必须跟着回退：这一帧没有上屏。
            diagnostics::present_retried();
            renderer.target.on_present_failed();
            return Err(FrameSubmitError::RetryFull);
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

#[cfg(test)]
mod tests {
    use super::{PresentRetry, invalid_call_recovery};
    use crate::{math::PhysicalRect, render::compositor::DamageRects};

    /// AR3 §4.2 缺口 1 / P2-4：`Present1(InvalidCall)` 之后**不允许**「整面重试」——
    /// 增量帧的 back buffer 只同步了 `pending ∪ damage`（整面 present 会把 3 帧前的旧内容 /
    /// fresh 时的未定义内容上屏），所以唯一安全的退路是整帧重录；整帧本身没有未同步区域，
    /// 「整面重试」只是把刚才失败的调用再发一遍，按致命处理。
    #[test]
    fn invalid_call_only_has_a_safe_retry_for_incremental_frames() {
        let mut damage = DamageRects::new();
        assert!(damage.push(PhysicalRect::new(0, 0, 8, 8)));
        assert_eq!(
            invalid_call_recovery(Some(&damage)),
            PresentRetry::FullRerecord,
            "增量帧必须整帧重录，不得整面 present"
        );
        assert_eq!(
            invalid_call_recovery(None),
            PresentRetry::Fatal,
            "整帧没有未同步区域：重试同一个调用没有意义"
        );
    }
}
