//! Linux 帧门禁：`wl_surface.frame` 回调的「已请求 / 已就绪」状态机。
//!
//! 门禁只负责一件事：上一帧的 `wl_callback.done` 回来之前不再向 GPU 线程发布下一帧，
//! 把在飞帧压到 ≤ 1（`design/async-render-pipeline.md` §7.5 第 3 条）。
//!
//! 两条既有缺陷会让门禁**永久关闭**（F-183）：
//! - **Idle 帧**：`App::frame` 返回 `FrameOutcome::Idle`（a11y-only 变化 / damage 全被裁空）
//!   时不 publish、不 commit，挂起的 callback 永远不会回来；
//! - **present `OUT_OF_DATE`**：WSI 可能没有 attach/commit，callback 同样不回来。
//!
//! 前者由 [`FrameGate::note_not_committed`] 立即重新开门（保留挂起 callback：下一次 commit
//! 会让它返回）；后者由 [`FrameGate::expire`] 的 200 ms **活性兜底**强制开门。超时不是呈现
//! 节奏：正常帧的 callback 在合成器 vsync 内返回，只有「publish 了但没 commit」才会走到。

use std::time::{Duration, Instant};

/// 门禁关闭后等待 callback 的活性上限（不是帧间隔；见模块文档）。
pub(crate) const GATE_TIMEOUT: Duration = Duration::from_millis(200);

/// 帧门禁状态（纯逻辑：不持有 `wl_surface`，单测不需要 Wayland 连接）。
pub(crate) struct FrameGate {
    /// 门禁打开：可以向 GPU 线程发布下一帧。
    ready: bool,
    /// 已向合成器请求、尚未返回的 `wl_callback`。
    requested: bool,
    /// 门禁关闭的时刻（活性兜底的计时起点）。
    closed_at: Option<Instant>,
}

impl FrameGate {
    pub(crate) const fn new() -> Self {
        Self {
            ready: true,
            requested: false,
            closed_at: None,
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    /// 请求一帧回调（必须在 publish **之前**调用）。返回 `true` 表示调用方要真的发一次
    /// `wl_surface.frame`；`false` = 已有挂起的 callback，只关好门禁（不重复请求）。
    ///
    /// 幂等是 Idle 路径的前提：Idle 会把门禁重新打开但保留挂起 callback，下一次请求不能再
    /// 挂一个 —— 挂起回调会在下一次 commit 后统一返回（多个 callback 本身合法，但没有必要
    /// 重复）。
    pub(crate) fn request(&mut self, now: Instant) -> bool {
        self.ready = false;
        self.closed_at = Some(now);
        if self.requested {
            return false;
        }
        self.requested = true;
        true
    }

    /// 本帧没有 publish（Idle / 中止）⇒ 不会 commit：立即重新开门，但**保留**挂起 callback。
    ///
    /// 不能清 `requested`：那个 callback 仍挂在 surface 上，下一次真正 commit 时会回来；
    /// 清掉它会让下一次请求再挂一个，Idle 循环会无限堆积 callback。
    pub(crate) fn note_not_committed(&mut self) {
        self.ready = true;
        self.closed_at = None;
    }

    /// `wl_callback.done`：挂起请求已完成，清标志并开门。
    pub(crate) fn on_callback(&mut self) {
        self.requested = false;
        self.ready = true;
        self.closed_at = None;
    }

    /// 活性兜底：门禁关闭超过 [`GATE_TIMEOUT`] 时强制开门。返回 `true` = 本次确实超时。
    ///
    /// 丢弃挂起记录：那个 callback 可能永远不会回来（present `OUT_OF_DATE` 没有 commit）。
    /// 若它之后真的回来了，`on_callback` 只是重复开门；下一次 `request` 会重新挂号。
    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        let Some(closed_at) = self.closed_at else {
            return false;
        };
        if now.duration_since(closed_at) < GATE_TIMEOUT {
            return false;
        }
        self.ready = true;
        self.requested = false;
        self.closed_at = None;
        true
    }

    /// 距离活性兜底还有多久（门禁打开时 `None`）；事件循环用它压缩阻塞等待。
    pub(crate) fn time_until_expiry(&self, now: Instant) -> Option<Duration> {
        let closed_at = self.closed_at?;
        Some(GATE_TIMEOUT.saturating_sub(now.duration_since(closed_at)))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{FrameGate, GATE_TIMEOUT};

    #[test]
    fn a_frame_request_is_idempotent_until_the_callback_returns() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        assert!(gate.is_ready());

        assert!(gate.request(start), "首次请求必须真的发 wl_surface.frame");
        assert!(!gate.is_ready());

        assert!(
            !gate.request(start + Duration::from_millis(5)),
            "挂起 callback 期间不得重复请求"
        );
        assert!(!gate.is_ready());

        gate.on_callback();
        assert!(gate.is_ready(), "callback 到达后门禁重新打开");
        assert!(
            gate.request(start + Duration::from_millis(10)),
            "callback 之后可以再次请求"
        );
    }

    #[test]
    fn an_idle_frame_reopens_the_gate_without_duplicating_the_callback() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        assert!(gate.request(start));

        gate.note_not_committed();
        assert!(gate.is_ready(), "Idle 没有 commit，门禁必须立即重开");
        assert!(
            !gate.request(start + Duration::from_millis(1)),
            "挂起 callback 仍在，不能重复请求"
        );
        assert!(!gate.is_ready());

        // 下一次 commit 让挂起的 callback 返回。
        gate.on_callback();
        assert!(gate.is_ready());
    }

    #[test]
    fn the_callback_clears_the_pending_request() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        gate.request(start);
        gate.note_not_committed();

        gate.on_callback();

        assert!(
            gate.request(start + Duration::from_millis(1)),
            "requested 已清 ⇒ 下一次请求必须重新挂号 callback"
        );
    }

    #[test]
    fn a_stuck_gate_expires_after_the_liveness_timeout() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        gate.request(start);

        assert_eq!(
            gate.time_until_expiry(start + Duration::from_millis(50)),
            Some(GATE_TIMEOUT - Duration::from_millis(50))
        );
        assert!(
            !gate.expire(start + GATE_TIMEOUT - Duration::from_millis(1)),
            "未到上限不兜底"
        );
        assert!(!gate.is_ready());

        assert!(
            gate.expire(start + GATE_TIMEOUT),
            "关闭满 200 ms ⇒ 活性兜底开门"
        );
        assert!(gate.is_ready());
        assert!(
            gate.request(start + GATE_TIMEOUT + Duration::from_millis(1)),
            "兜底后必须重新挂号 callback"
        );
    }

    #[test]
    fn an_open_gate_never_expires() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        assert!(!gate.expire(start + GATE_TIMEOUT), "门禁已打开");
        gate.request(start);
        gate.note_not_committed();
        assert!(
            !gate.expire(start + GATE_TIMEOUT * 2),
            "门禁已打开，没有可兜底的状态"
        );
        assert!(gate.is_ready());
    }
}
