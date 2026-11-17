//! Wayland 帧门禁：`wl_surface.frame` 回调的「已请求 / 已就绪」状态机（R10-7）。
//!
//! 门禁只负责一件事：上一帧的 `wl_callback.done` 回来之前不再向 GPU 线程发布下一帧，
//! 把在飞帧压到 ≤ 1（`design/async-render-pipeline.md` §7.5 第 3 条）。
//!
//! # 没有超时兜底（AR3 §3.7）
//!
//! 旧实现另有一条 200 ms「活性兜底」：门禁关闭满 200 ms 就强制开门。它在本设计里是**多余的**，
//! 而且一旦某类窗口（例如「可见但被完全遮挡」——surface 仍 mapped、永远收不到 `leave`）长期
//! 拿不到 callback，就会退化成 5 Hz 全量重绘 + 持续 commit，正是 R10-7 的病原。删除它的依据：
//!
//! - **不可见的 surface 根本不请求帧**（`WaylandState::is_visible`：`xdg_toplevel.configure`
//!   的 `Suspended` + `wl_surface.enter/leave`）。协议允许服务端在 surface 不可见时不回
//!   callback，所以「不可见」在门禁这一层不会留下「关闭中」的状态：转不可见时直接
//!   [`FrameGate::force_idle`]（`wl_surface.leave` 与 `xdg_toplevel.configure` 两个入口）。
//! - **可见窗口的 callback 一定会回来**：`FrameOutcome::Committed` ⇒ 帧已 publish（V-122 ⑧ /
//!   F-183），GPU 线程 present 时必然 `wl_surface.commit()`；present 侧的
//!   `VK_ERROR_OUT_OF_DATE_KHR` / `NOT_READY` 由 GPU 线程**保留待画帧重试**
//!   （`render/linux/renderer.rs` 的 `Submitted::Retry`），下一次 acquire 成功就会 commit。
//! - 唯一「publish 了但不会 commit」的路径是 Idle / 中止帧（不 publish），由
//!   [`FrameGate::note_not_committed`] 立即重开 —— 顺势保留挂起 callback（Idle 没有 commit，
//!   那个 callback 仍挂在 surface 上，下一次真正 commit 时返回；清掉它会让下一次请求再挂一个，
//!   Idle 循环会无限堆积 callback）。
//!
//! 于是「可见 ⇒ 门禁在有限时间内打开」由**协议 + GPU 线程的保帧重试**共同保证，客户端不需要
//! 自旋。
//!
//! ## 已知取舍：「可见但被完全遮挡」
//!
//! `wayland.xml:1649`（request `frame`）原文：*"A server **should avoid** signaling the frame
//! callbacks if the surface is not visible in any way, e.g. the surface is off-screen, or
//! **completely obscured by other opaque surfaces**."* —— 被别的窗口挡住时 surface 仍 mapped、
//! 也不会有 `leave`，compositor 会**推迟**那条 callback。本设计因此**必须**假设"重新可见时服务端
//! 补发已挂起的 callback"（mutter / wlroots 都是推迟而非丢弃）。若某个 compositor 永久丢弃：
//! 窗口停在最后一帧 —— 画面仍正确（那正是它最后 present 的内容），只是不再响应新的重绘。
//!
//! 这是**有意**的取舍：恢复 200 ms 周期兜底会把「挂机时被遮挡的窗口」变回 5 Hz 全量重绘
//! （R10-7 的原病），代价远大于这个边缘情形。真机验证项见整改报告 §5（A3/A4、B4）；
//! 若真机证实会丢，回退方案是"仅在『可见 + 门禁关闭且无新帧』时启用**一次**兜底"，而不是
//! 恢复周期计时。
//!
//! # 迟到的 callback 不得改写新请求（F-183 复审）
//!
//! [`FrameGate::force_idle`] 会放弃挂起记录（那个 callback 可能永远不会回来），随后
//! `request` 再挂一个新的；如果旧 callback 后来真的返回，无条件的「清挂起 + 开门」会把新请求
//! 的 pending 清掉，下一次 `request` 又能再挂一个，门禁「最多一个挂起 callback」的约束被破坏。
//! 因此每次挂 callback 都发一个 [`CallbackToken`]，[`FrameGate::on_callback`] 只接受当前令牌：
//! 迟到的旧令牌直接忽略。**计数模型做不到这一点**（旧 callback 与新 callback 都只能让计数 -1，
//! 无法区分归属）；它要么让迟到的旧 callback 消耗掉新请求的计数，要么在 Idle 路径上无限堆积。

/// 一次 `wl_surface.frame` 请求的令牌（迟到 callback 的识别依据）。
///
/// 令牌只由 [`FrameGate::request`] 发放；[`FrameGate::on_callback`] 只接受与当前挂起令牌
/// 相同的回调，因此被 `force_idle` 放弃的旧回调不会影响后来重新挂上的请求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CallbackToken(u64);

/// 帧门禁状态（纯逻辑：不持有 `wl_surface`，单测不需要 Wayland 连接）。
pub(crate) struct FrameGate {
    /// 门禁打开：可以向 GPU 线程发布下一帧。
    ready: bool,
    /// 当前**有效**的挂起 callback；`force_idle` 放弃它之后是 `None`（旧 callback 迟到即无主）。
    pending: Option<CallbackToken>,
    /// 下一次 `request` 发放的令牌（单调递增，一次运行内不重复）。
    next_token: u64,
}

impl FrameGate {
    pub(crate) const fn new() -> Self {
        Self {
            ready: true,
            pending: None,
            next_token: 0,
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
    pub(crate) fn request(&mut self) -> Option<CallbackToken> {
        self.ready = false;
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
    }

    /// `wl_callback.done`：只有**当前**请求的 callback 才清挂起并开门。
    ///
    /// 被 `force_idle` 放弃的旧 callback 迟到时令牌不匹配，直接忽略（不得动新请求）；
    /// 同一个 callback 重复到达也只生效一次。
    pub(crate) fn on_callback(&mut self, token: CallbackToken) {
        if self.pending != Some(token) {
            return;
        }
        self.pending = None;
        self.ready = true;
    }

    /// 窗口不可见（R10-7）：门禁直接回到 Idle 状态，**不**留挂起记录。
    ///
    /// 协议允许服务端在 surface 不可见时不回 frame callback；等窗口重新可见时由 `request` 发
    /// 一个新的 callback —— 那个旧 callback 可能永远不会回来了（迟到的旧令牌会被
    /// [`Self::on_callback`] 忽略，语义安全）。
    pub(crate) fn force_idle(&mut self) {
        self.ready = true;
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::FrameGate;

    #[test]
    fn a_frame_request_is_idempotent_until_the_callback_returns() {
        let mut gate = FrameGate::new();
        assert!(gate.is_ready());

        let first = gate.request().expect("首次请求必须真的发 wl_surface.frame");
        assert!(!gate.is_ready());

        assert!(gate.request().is_none(), "挂起 callback 期间不得重复请求");
        assert!(!gate.is_ready());

        gate.on_callback(first);
        assert!(gate.is_ready(), "callback 到达后门禁重新打开");
        assert!(gate.request().is_some(), "callback 之后可以再次请求");
    }

    #[test]
    fn an_idle_frame_reopens_the_gate_without_duplicating_the_callback() {
        let mut gate = FrameGate::new();
        let callback = gate.request().expect("首次请求挂起 callback");

        gate.note_not_committed();
        assert!(gate.is_ready(), "Idle 没有 commit，门禁必须立即重开");
        assert!(gate.request().is_none(), "挂起 callback 仍在，不能重复请求");
        assert!(!gate.is_ready());

        // 下一次 commit 让挂起的 callback 返回。
        gate.on_callback(callback);
        assert!(gate.is_ready());
    }

    #[test]
    fn the_callback_clears_the_pending_request() {
        let mut gate = FrameGate::new();
        let callback = gate.request().expect("首次请求挂起 callback");
        gate.note_not_committed();

        gate.on_callback(callback);

        assert!(
            gate.request().is_some(),
            "pending 已清 ⇒ 下一次请求必须重新挂号 callback"
        );
    }

    /// AR3 §3.7（P2）：**没有**任何基于时间的兜底 —— 门禁关闭之后必须等 callback，
    /// 中间的时间流逝不改变状态、也不允许重复挂 callback。
    ///
    /// 旧实现有一条 200 ms 的 `expire`：可见但被完全遮挡的窗口（收不到 `leave`、
    /// compositor 也不回 callback）会每 200 ms 被强制开门一次 ⇒ 5 Hz 全量重绘。
    #[test]
    fn a_closed_gate_waits_for_the_callback_instead_of_a_timeout() {
        let mut gate = FrameGate::new();
        let callback = gate.request().expect("首次请求挂起 callback");

        // 模拟「已经过了很久、callback 一直没回来」：没有任何入口能让门禁自己打开。
        assert!(!gate.is_ready());
        assert!(
            gate.request().is_none(),
            "时间流逝不得允许重复挂 callback（否则每个 200 ms 就是一次全量重绘）"
        );
        assert!(!gate.is_ready(), "没有 callback 就不能重开");

        gate.on_callback(callback);
        assert!(gate.is_ready(), "只有 callback 才开门");
    }

    /// P1 回归：`force_idle` 之后新挂的请求，不得被迟到（被放弃）的旧 callback 清掉。
    ///
    /// 旧实现（`requested: bool`）里 `force_idle` 清标志、`request` 再置位；旧 callback 迟到时
    /// `on_callback` 无条件清标志并开门，于是第 3 次 `request` 又能挂一个 callback（同时在飞
    /// 两个），门禁的「最多一个挂起 callback」被破坏。
    #[test]
    fn a_late_callback_after_force_idle_does_not_clear_the_new_request() {
        let mut gate = FrameGate::new();
        let stale = gate.request().expect("首次请求挂起 callback");

        gate.force_idle();
        assert!(gate.is_ready());
        assert_eq!(gate.pending, None, "不可见时放弃旧 callback，允许重新挂号");

        let live = gate
            .request()
            .expect("重新可见之后必须能重新挂一个 callback");
        assert_ne!(live, stale);
        assert!(!gate.is_ready());

        // 旧的 callback 迟到（不可见期间被放弃，之后合成器又补发了 done）。
        gate.on_callback(stale);
        assert_eq!(
            gate.pending,
            Some(live),
            "迟到的旧 callback 不得清掉新请求的挂起状态"
        );
        assert!(!gate.is_ready(), "新 callback 未返回，门禁不得提前打开");
        assert!(
            gate.request().is_none(),
            "仍有 1 个挂起 ⇒ 第 3 次 request 不能再挂一个"
        );

        gate.on_callback(live);
        assert!(gate.is_ready(), "新 callback 返回后才开门");
        assert_eq!(gate.pending, None);
    }

    /// R10-7：窗口不可见时门禁回到 Idle —— 不留挂起记录（重新可见时可以立即发一个新的
    /// `wl_surface.frame`），也不留下任何会被唤醒的计时状态。
    #[test]
    fn an_invisible_window_drops_the_gate_without_leaving_a_request() {
        let mut gate = FrameGate::new();
        let stale = gate.request().expect("首次请求挂起 callback");

        gate.force_idle();
        assert!(gate.is_ready());
        assert_eq!(gate.pending, None);

        let fresh = gate.request().expect("重新可见后必须能挂新 callback");
        assert_ne!(fresh, stale, "每次挂号都是新令牌");
        gate.on_callback(stale);
        assert!(
            !gate.is_ready(),
            "旧的（不可见期间被放弃的）callback 迟到不得提前开门"
        );
        gate.on_callback(fresh);
        assert!(gate.is_ready());
    }
}
