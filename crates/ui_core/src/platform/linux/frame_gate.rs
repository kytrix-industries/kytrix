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
//!
//! **迟到的 callback 不得改写新请求**（F-183 复审）：兜底会放弃旧 callback（它可能永远不回来），
//! 随后 `request` 再挂一个新的；如果旧 callback 后来真的返回，无条件的「清挂起 + 开门」会把
//! 新请求的 pending 清掉，下一次 `request` 又能再挂一个，门禁「最多一个挂起 callback」的约束
//! 被破坏。因此每次挂 callback 都发一个 [`CallbackToken`]，`on_callback` 只接受当前令牌：迟到的
//! 旧令牌直接忽略。**计数模型做不到这一点**（旧 callback 与新 callback 都只能让计数 -1，无法
//! 区分归属）；它要么让迟到的旧 callback 消耗掉新请求的计数，要么在 Idle 路径上无限堆积。

use std::time::{Duration, Instant};

/// 门禁关闭后等待 callback 的活性上限（不是帧间隔；见模块文档）。
pub(crate) const GATE_TIMEOUT: Duration = Duration::from_millis(200);

/// 一次 `wl_surface.frame` 请求的令牌（迟到 callback 的识别依据）。
///
/// 令牌只由 [`FrameGate::request`] 发放；[`FrameGate::on_callback`] 只接受与当前挂起令牌
/// 相同的回调，因此被 `expire` 放弃的旧回调不会影响后来重新挂上的请求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallbackToken(u64);

/// 帧门禁状态（纯逻辑：不持有 `wl_surface`，单测不需要 Wayland 连接）。
pub(crate) struct FrameGate {
    /// 门禁打开：可以向 GPU 线程发布下一帧。
    ready: bool,
    /// 当前**有效**的挂起 callback；`expire` 放弃它之后是 `None`（旧 callback 迟到即无主）。
    pending: Option<CallbackToken>,
    /// 下一次 `request` 发放的令牌（单调递增，一次运行内不重复）。
    next_token: u64,
    /// 门禁关闭的时刻（活性兜底的计时起点）。
    closed_at: Option<Instant>,
}

impl FrameGate {
    pub(crate) const fn new() -> Self {
        Self {
            ready: true,
            pending: None,
            next_token: 0,
            closed_at: None,
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready
    }

    /// 请求一帧回调（必须在 publish **之前**调用）。
    ///
    /// 返回 `Some(token)` = 调用方要真的发一次 `wl_surface.frame`，并把 token 交给回调分发；
    /// `None` = 已有挂起的 callback，只关好门禁（不重复请求）。
    ///
    /// 幂等是 Idle 路径的前提：Idle 会把门禁重新打开但保留挂起 callback，下一次请求不能再
    /// 挂一个 —— 挂起回调会在下一次 commit 后统一返回（多个 callback 本身合法，但没有必要
    /// 重复）。
    pub(crate) fn request(&mut self, now: Instant) -> Option<CallbackToken> {
        self.ready = false;
        self.closed_at = Some(now);
        if self.pending.is_some() {
            return None;
        }
        let token = CallbackToken(self.next_token);
        self.next_token += 1;
        self.pending = Some(token);
        Some(token)
    }

    /// 本帧没有 publish（Idle / 中止）⇒ 不会 commit：立即重新开门，但**保留**挂起 callback。
    ///
    /// 不能清 `pending`：那个 callback 仍挂在 surface 上，下一次真正 commit 时会回来；
    /// 清掉它会让下一次请求再挂一个，Idle 循环会无限堆积 callback。
    pub(crate) fn note_not_committed(&mut self) {
        self.ready = true;
        self.closed_at = None;
    }

    /// `wl_callback.done`：只有**当前**请求的 callback 才清挂起并开门。
    ///
    /// 被 [`Self::expire`] 放弃的旧 callback 迟到时令牌不匹配，直接忽略（不得动新请求）；
    /// 同一个 callback 重复到达也只生效一次。
    pub(crate) fn on_callback(&mut self, token: CallbackToken) {
        if self.pending != Some(token) {
            return;
        }
        self.pending = None;
        self.ready = true;
        self.closed_at = None;
    }

    /// 活性兜底：门禁关闭超过 [`GATE_TIMEOUT`] 时强制开门。返回 `true` = 本次确实超时。
    ///
    /// 放弃挂起记录：那个 callback 可能永远不会回来（present `OUT_OF_DATE` 没有 commit）。
    /// 若它之后真的回来了，令牌已经不匹配 ⇒ `on_callback` 忽略；下一次 `request` 发新令牌。
    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        let Some(closed_at) = self.closed_at else {
            return false;
        };
        if now.duration_since(closed_at) < GATE_TIMEOUT {
            return false;
        }
        self.ready = true;
        self.pending = None;
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

        let first = gate
            .request(start)
            .expect("首次请求必须真的发 wl_surface.frame");
        assert!(!gate.is_ready());

        assert!(
            gate.request(start + Duration::from_millis(5)).is_none(),
            "挂起 callback 期间不得重复请求"
        );
        assert!(!gate.is_ready());

        gate.on_callback(first);
        assert!(gate.is_ready(), "callback 到达后门禁重新打开");
        assert!(
            gate.request(start + Duration::from_millis(10)).is_some(),
            "callback 之后可以再次请求"
        );
    }

    #[test]
    fn an_idle_frame_reopens_the_gate_without_duplicating_the_callback() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        let callback = gate.request(start).expect("首次请求挂起 callback");

        gate.note_not_committed();
        assert!(gate.is_ready(), "Idle 没有 commit，门禁必须立即重开");
        assert!(
            gate.request(start + Duration::from_millis(1)).is_none(),
            "挂起 callback 仍在，不能重复请求"
        );
        assert!(!gate.is_ready());

        // 下一次 commit 让挂起的 callback 返回。
        gate.on_callback(callback);
        assert!(gate.is_ready());
    }

    #[test]
    fn the_callback_clears_the_pending_request() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        let callback = gate.request(start).expect("首次请求挂起 callback");
        gate.note_not_committed();

        gate.on_callback(callback);

        assert!(
            gate.request(start + Duration::from_millis(1)).is_some(),
            "pending 已清 ⇒ 下一次请求必须重新挂号 callback"
        );
    }

    #[test]
    fn a_stuck_gate_expires_after_the_liveness_timeout() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        let stale = gate.request(start).expect("首次请求挂起 callback");

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
        let live = gate
            .request(start + GATE_TIMEOUT + Duration::from_millis(1))
            .expect("兜底后必须重新挂号 callback");
        assert_ne!(live, stale, "每次挂号都必须是新令牌");
    }

    /// P1 回归：兜底放弃的旧 callback 迟到时，不得清掉兜底之后新挂的请求。
    ///
    /// 旧实现（`requested: bool`）里 `expire` 清标志、`request` 再置位；旧 callback 迟到时
    /// `on_callback` 无条件清标志并开门，于是第 3 次 `request` 又能挂一个 callback（同时在飞
    /// 两个），门禁的「最多一个挂起 callback」被破坏。
    #[test]
    fn a_late_callback_after_the_liveness_expiry_does_not_clear_the_new_request() {
        let start = Instant::now();
        let mut gate = FrameGate::new();
        let stale = gate.request(start).expect("首次请求挂起 callback");

        assert!(gate.expire(start + GATE_TIMEOUT), "卡死的门禁被兜底打开");
        assert!(gate.is_ready());
        assert_eq!(gate.pending, None, "兜底放弃旧 callback，允许重新挂号");

        let live = gate
            .request(start + GATE_TIMEOUT + Duration::from_millis(1))
            .expect("兜底之后必须能重新挂一个 callback");
        assert_ne!(live, stale);
        assert!(!gate.is_ready());

        // 旧的 callback 迟到（present OUT_OF_DATE 没有 commit，但合成器后来补发了 done）。
        gate.on_callback(stale);
        assert_eq!(
            gate.pending,
            Some(live),
            "迟到的旧 callback 不得清掉新请求的挂起状态"
        );
        assert!(!gate.is_ready(), "新 callback 未返回，门禁不得提前打开");
        assert!(
            gate.request(start + GATE_TIMEOUT + Duration::from_millis(2))
                .is_none(),
            "仍有 1 个挂起 ⇒ 第 3 次 request 不能再挂一个"
        );

        gate.on_callback(live);
        assert!(gate.is_ready(), "新 callback 返回后才开门");
        assert_eq!(gate.pending, None);
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
