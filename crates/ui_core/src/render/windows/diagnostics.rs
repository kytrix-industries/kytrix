//! 后端的运行期计数器（R08-3 / R08-7 / R08-13 的可观测面）。
//!
//! 共享层的 `RenderStats` 是跨平台契约（不改它的字段），且只表达「每帧画了多少」；这里放
//! **只有 Windows 后端有意义**的进程级计数，经 [`crate::render::windows::renderer::Renderer::diagnostics`]
//! 提供给 HUD / 日志 / 测试断言。
//!
//! 用原子量而不是 `Mutex`：写入点在 GPU 线程的热路径上（每帧 0–2 次），读取在 UI 线程，
//! 冲突概率极低且 `Relaxed` 足够（这些值只用于诊断，不参与任何正确性判定）。

use std::sync::atomic::{AtomicU64, Ordering};

/// 一次快照（进程级累计值；多窗口时是各窗口之和）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Diagnostics {
    /// fence 等待超时次数（**设备健康**时不再升级成 `DeviceLost`，只计数并退避）。
    pub frame_wait_timeouts: u64,
    /// present waitable 等待超时次数（锁屏 / 熄屏 / RDP / DWM 停摆）。
    pub present_waitable_timeouts: u64,
    /// `Present1` 返回**负** HRESULT 的次数（不含可重试的 `InvalidCall`）。
    pub present_failed: u64,
    /// `Present1` 失败后用全量 dirty 重试的次数（R08-7）。
    pub present_retries: u64,
    /// 遮挡期间被跳过的待画帧数（R08-7：不取帧、不提交、不 present）。
    pub occluded_skips: u64,
    /// 设备丢失后**进程内**恢复成功的次数（R08-4）。
    pub device_recoveries: u64,
    /// 图集换代时因为等不到某个窗口的水位而推迟销毁旧纹理的次数（R08-6 的慢路径）。
    pub atlas_deferred_rebuilds: u64,
    /// `ResizeBuffers` 失败后回退到整链替换的次数（R08-1 的回退开关）。
    pub resize_fallbacks: u64,
    /// 局部帧实际访问的 op 数（R08-13：目标是「≈ 变更图元数」，而不是 `ops × passes`）。
    pub op_visits: u64,
    /// 同一批帧的「整份重放」预算（`ops.len() × damage 数`），用于和 `op_visits` 对比。
    pub op_replay_budget: u64,
    /// D3D12 调试层消息计数（R08-5；release 构建恒 0）。
    pub debug_warnings: u64,
    pub debug_errors: u64,
    pub debug_corruption: u64,
}

static FRAME_WAIT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static PRESENT_WAITABLE_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static PRESENT_FAILED: AtomicU64 = AtomicU64::new(0);
static PRESENT_RETRIES: AtomicU64 = AtomicU64::new(0);
static OCCLUDED_SKIPS: AtomicU64 = AtomicU64::new(0);
static DEVICE_RECOVERIES: AtomicU64 = AtomicU64::new(0);
static ATLAS_DEFERRED_REBUILDS: AtomicU64 = AtomicU64::new(0);
static RESIZE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static OP_VISITS: AtomicU64 = AtomicU64::new(0);
static OP_REPLAY_BUDGET: AtomicU64 = AtomicU64::new(0);

/// 记一次 fence 超时（设备健康时只是一次退避，不是错误）。
pub(crate) fn frame_wait_timeout() {
    FRAME_WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

/// 记一次 present waitable 超时。
pub(crate) fn present_waitable_timeout() {
    PRESENT_WAITABLE_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}

/// 记一次 `Present1` 负 HRESULT。
pub(crate) fn present_failed() {
    PRESENT_FAILED.fetch_add(1, Ordering::Relaxed);
}

/// 记一次 `Present1` 的全量重试。
pub(crate) fn present_retried() {
    PRESENT_RETRIES.fetch_add(1, Ordering::Relaxed);
}

/// 记一次遮挡跳过。
pub(crate) fn occluded_skipped() {
    OCCLUDED_SKIPS.fetch_add(1, Ordering::Relaxed);
}

/// 记一次设备恢复。
pub(crate) fn device_recovered() {
    DEVICE_RECOVERIES.fetch_add(1, Ordering::Relaxed);
}

/// 记一次图集换代的慢路径（旧纹理进延迟销毁表）。
pub(crate) fn atlas_deferred_rebuild() {
    ATLAS_DEFERRED_REBUILDS.fetch_add(1, Ordering::Relaxed);
}

/// 记一次 `ResizeBuffers` 失败后的整链回退。
pub(crate) fn resize_fallback() {
    RESIZE_FALLBACKS.fetch_add(1, Ordering::Relaxed);
}

/// 记局部帧的 op 访问量：`visited` = 实际遍历的 op 数，`budget` = 整份重放会遍历的数量。
pub(crate) fn op_visits(visited: u32, budget: u32) {
    OP_VISITS.fetch_add(u64::from(visited), Ordering::Relaxed);
    OP_REPLAY_BUDGET.fetch_add(u64::from(budget), Ordering::Relaxed);
}

/// 读取全部计数（`Acquire` 不必要：诊断值之间没有相互依赖）。
pub(crate) fn snapshot() -> Diagnostics {
    let debug = super::debug::message_counts();
    Diagnostics {
        frame_wait_timeouts: FRAME_WAIT_TIMEOUTS.load(Ordering::Relaxed),
        present_waitable_timeouts: PRESENT_WAITABLE_TIMEOUTS.load(Ordering::Relaxed),
        present_failed: PRESENT_FAILED.load(Ordering::Relaxed),
        present_retries: PRESENT_RETRIES.load(Ordering::Relaxed),
        occluded_skips: OCCLUDED_SKIPS.load(Ordering::Relaxed),
        device_recoveries: DEVICE_RECOVERIES.load(Ordering::Relaxed),
        atlas_deferred_rebuilds: ATLAS_DEFERRED_REBUILDS.load(Ordering::Relaxed),
        resize_fallbacks: RESIZE_FALLBACKS.load(Ordering::Relaxed),
        op_visits: OP_VISITS.load(Ordering::Relaxed),
        op_replay_budget: OP_REPLAY_BUDGET.load(Ordering::Relaxed),
        debug_warnings: debug.warning,
        debug_errors: debug.error,
        debug_corruption: debug.corruption,
    }
}

/// 一行诊断（日志 / HUD；不含时间戳，便于 A-B 对照）。
impl Diagnostics {
    pub(crate) fn describe(&self) -> smol_str::SmolStr {
        smol_str::format_smolstr!(
            "wait_timeouts={} present_waitable_timeouts={} present_failed={} present_retries={} occluded_skips={} device_recoveries={} atlas_deferred={} resize_fallbacks={} op_visits={}/{} debug(w/e/c)={}/{}/{}",
            self.frame_wait_timeouts,
            self.present_waitable_timeouts,
            self.present_failed,
            self.present_retries,
            self.occluded_skips,
            self.device_recoveries,
            self.atlas_deferred_rebuilds,
            self.resize_fallbacks,
            self.op_visits,
            self.op_replay_budget,
            self.debug_warnings,
            self.debug_errors,
            self.debug_corruption
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Diagnostics, snapshot};

    /// 计数是进程级累计量：快照必须单调且包含调试层字段（`describe` 不 panic）。
    ///
    /// 这里刻意**不**碰 `op_visits`：那个计数被 GPU 测试按精确增量断言（R08-13），并行测试
    /// 同时递增会让它变成 flaky。
    #[test]
    fn the_snapshot_is_monotonic_and_describable() {
        let first = snapshot();
        super::frame_wait_timeout();
        super::occluded_skipped();
        super::present_retried();
        let second = snapshot();
        assert!(second.frame_wait_timeouts > first.frame_wait_timeouts);
        assert!(second.occluded_skips > first.occluded_skips);
        assert!(second.present_retries > first.present_retries);

        let text = second.describe();
        for needle in [
            "wait_timeouts=",
            "op_visits=",
            "occluded_skips=",
            "debug(w/e/c)=",
        ] {
            assert!(text.contains(needle), "missing {needle} in {text}");
        }
        assert_eq!(Diagnostics::default().op_visits, 0);
    }
}
