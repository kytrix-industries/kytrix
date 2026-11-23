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

pub(crate) enum FrameSubmitError {
    Fatal(EngineError),
    NeedsResize,
    RetryFull,
}

impl FrameSubmitError {
    fn fatal(error: EngineError) -> Self {
        Self::Fatal(error)
    }
}

pub(crate) fn invalid_call_recovery(presented: Option<&DamageRects>) -> PresentRetry {
    match presented {
        Some(_) => PresentRetry::FullRerecord,
        None => PresentRetry::Fatal,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentRetry {
    FullRerecord,
    Fatal,
}

pub(super) fn finish_sync(
    renderer: &mut GpuRenderer,
    slot: FrameSlot,
    timings: &mut FrameTimings,
) -> Result<(), EngineError> {
    if !renderer
        .stream
        .wait_slot_fence(slot.signal_value)
        .map_err(|error| renderer.classify(error))?
        .is_signaled()
    {
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
        let sync = if replace { 0 } else { 1 };
        outcome = swapchain.present1(recorded.present.as_ref(), sync);
        if outcome == PresentOutcome::InvalidCall
            && invalid_call_recovery(recorded.present.as_ref()) == PresentRetry::FullRerecord
        {
            diagnostics::present_retried();
            renderer.target.on_present_failed();
            return Err(FrameSubmitError::RetryFull);
        }
    }
    debug_assert!(
        !frame.is_full() || recorded.present.is_none(),
        "a full frame must present the whole surface"
    );
    match outcome {
        PresentOutcome::Presented | PresentOutcome::Clipped => {
            renderer.target.on_presented();
            frame.presented();
        }
        PresentOutcome::Occluded => {
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

pub(super) fn elapsed_ns(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{PresentRetry, invalid_call_recovery};
    use crate::{math::PhysicalRect, render::compositor::DamageRects};

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
