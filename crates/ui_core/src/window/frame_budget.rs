//! 每 refresh 周期最多一帧的预算门禁（A5 / §3.11 P1-3）。
//!
//! **问题**：HEAD 的 `App::frame` 没有任何时间门禁 —— `Dispatcher::wake` 的电平位只合并
//! 「同一时刻」的重复请求，泵每处理一条消息就跑一次帧，于是 UI 帧率 ≈ 唤醒率（每秒数百次
//! 行情写入可把 UI 线程打满）。三端语义还分叉：Linux 有逐窗口 `wl_callback` 门禁，macOS 的
//! display link tick 只对连续帧生效，Windows 什么都没有。
//!
//! **方案（用户裁决）**：在 `App::frame` 入口加统一的「每 refresh 周期最多一帧」预算门禁，
//! 快于刷新的状态写入合并到下一 tick 出帧（最多晚 1 个 vsync）。`force` 通道保留
//! （resize / move 模态循环 / 动画 / 合成线程唤醒不受预算约束）。
//!
//! 对照实现：
//! - Chromium：`cc::Scheduler` 的 `BeginFrame` 由每 Display 的 `BeginFrameSource` 驱动，
//!   输入/动画的 `SetNeedsBeginFrame` 只置「需要一帧」，真正的帧在下一拍合并（本模块的
//!   `next_allowed` 就是该语义的最小实现）；
//! - Qt 6：`QWindow::requestUpdate()` 用平台 tick / 5 ms 定时器合并多次 `update()` 请求；
//! - 与 A2/A3 的分工：本模块只管 **UI 线程的提交起点节流**（CPU 预算）；present 节奏仍由
//!   每窗口 GPU 门禁（DXGI waitable / CAMetalDisplayLink / `wl_surface.frame`）决定。
//!
//! 本类型是**纯状态机**（手动注入 `Instant` 可单测），不读时钟、不碰平台。

use std::time::{Duration, Instant};

/// 刷新率下限（Hz）：低于它的提示视为不可信，禁用门禁。
///
/// 24 Hz 以下是远程桌面 / 省电模式的极端值，按「未知」处理比按 24 Hz 卡帧更安全。
const MIN_REFRESH_HZ: f32 = 24.0;
/// 刷新率上限（Hz）：上限 480 覆盖 ProMotion 120 / 高刷 360 / 已上市 480 Hz 显示器；
/// 超范围同样禁用门禁（退回唤醒即出帧，绝不用错误周期卡帧）。
const MAX_REFRESH_HZ: f32 = 480.0;

/// 窗口级帧预算门禁（`Window` 的私有字段；平台层通过 `Window::set_frame_refresh_hz` 注入
/// 刷新率提示）。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FrameBudget {
    /// 一个 refresh 周期；`None` = 门禁未启用（headless / 刷新率未知 / 提示非法）。
    interval: Option<Duration>,
    /// 下一次允许开始一帧的时刻（`None` = 还没有放行过帧）。
    next_allowed: Option<Instant>,
    /// 累计被推迟的帧请求（诊断与验收；单调递增，不随窗口生命周期清零）。
    deferrals: u64,
    /// 上一次推迟还没有被放行：平台据此决定是否挂一次性唤醒（避免空转）。
    deferred: bool,
}

impl FrameBudget {
    /// 平台注入「窗口所在 output」的刷新率提示。
    ///
    /// 非法值（非有限 / 越界）⇒ 门禁保持禁用：宁可回到「唤醒即出帧」也不能用一个错误周期
    /// 把帧卡死。提示**变化**时丢掉旧相位（换屏 / 改刷新率 / 显示恢复后立即放行一帧），
    /// 同值写入是完全幂等的（平台每轮 / 每次恢复都会调用）。
    pub(crate) fn set_refresh_hz(&mut self, hz: f32) {
        let interval = if hz.is_finite() && (MIN_REFRESH_HZ..=MAX_REFRESH_HZ).contains(&hz) {
            Some(Duration::from_secs_f64(1.0 / f64::from(hz)))
        } else {
            None
        };
        if interval != self.interval {
            self.interval = interval;
            self.next_allowed = None;
            self.deferred = false;
        }
    }

    /// 当前生效的刷新率提示（验收与 HUD 的诊断面）。
    pub(crate) fn refresh_hz(&self) -> Option<f32> {
        self.interval
            .map(|interval| 1.0 / interval.as_secs_f64() as f32)
    }

    /// 本帧是否放行：`forced`（force 通道）、门禁未启用、或已到下一拍。
    ///
    /// 相位带 **10% 周期的提前量**：平台唤醒 / 线程调度有 0.5–1.5 ms 抖动，严格相等会让
    /// 「与刷新同频」的写入有一半落在 `next_allowed` 前几百微秒而被多合并整整一拍（实测
    /// 60 Hz 写入掉到 ~41 fps）。提前量把稳态上限抬到 1.11×刷新率，但保证同频写入稳定
    /// 60 fps；Chromium 的 BeginFrame 调度同样带 deadline 容差，Qt 的 `requestUpdate`
    /// 也有 ~5 ms 合并窗口。
    pub(crate) fn admits(&self, now: Instant, forced: bool) -> bool {
        if forced {
            return true;
        }
        let Some(interval) = self.interval else {
            return true;
        };
        self.next_allowed
            .is_none_or(|next| now + interval / 10 >= next)
    }

    /// 放行一帧：记下本帧起点并推进下一拍；同时清掉「推迟未补」标记。
    pub(crate) fn note_start(&mut self, now: Instant) {
        self.deferred = false;
        if let Some(interval) = self.interval {
            self.next_allowed = Some(now + interval);
        }
    }

    /// 记一次推迟（返回 `true` = 这是一笔**新的**推迟，平台应挂一次性唤醒）。
    pub(crate) fn note_deferred(&mut self) -> bool {
        self.deferrals = self.deferrals.saturating_add(1);
        let first = !self.deferred;
        self.deferred = true;
        first
    }

    /// 是否有「已推迟、还没放行」的帧请求。
    pub(crate) fn is_deferred(&self) -> bool {
        self.deferred
    }

    /// 清除「推迟未补」标记（窗口不可见 / 最小化时调用）。
    ///
    /// **RA-5**：推迟帧的一次性唤醒依赖「有窗口 `deferred == true`」；若窗口随即不可见，平台
    /// 帧循环会跳过它 ⇒ 标记永不清除，mac 连续帧定时器常驻、Linux calloop timeout=0 空转。
    /// 不可见窗口不再需要补帧（可见性恢复路径会置全量 damage + 请求一帧），因此这里只清标记、
    /// 不伪造放行（脏位保留）；累计推迟计数不清（诊断量）。
    pub(crate) fn clear_deferred(&mut self) {
        self.deferred = false;
    }

    /// 被推迟的帧最早何时可以开始（Linux calloop timeout 的到期时刻）。
    #[cfg(target_os = "linux")]
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.deferred.then_some(self.next_allowed).flatten()
    }

    /// 累计推迟次数（验收诊断）。
    pub(crate) fn deferrals(&self) -> u64 {
        self.deferrals
    }

    /// 仅测试：直接注入一个周期（绕过 24–480 Hz 的合法性检查），让预算测试不依赖真实刷新率。
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn set_test_interval(&mut self, interval: Option<Duration>) {
        self.interval = interval;
        self.next_allowed = None;
        self.deferred = false;
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::FrameBudget;

    fn budget(hz: f32) -> FrameBudget {
        let mut budget = FrameBudget::default();
        budget.set_refresh_hz(hz);
        budget
    }

    /// **P1A-5a 的核心**：同一个 refresh 周期内第二次请求必须被推迟，周期末尾放行。
    #[test]
    fn a_second_request_inside_the_refresh_period_is_deferred() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        assert!(budget.admits(t0, false), "第一帧必须放行");
        budget.note_start(t0);

        assert!(
            !budget.admits(t0 + Duration::from_millis(8), false),
            "同一 refresh 周期内的第二次请求必须被推迟"
        );
        assert!(
            budget.admits(t0 + Duration::from_millis(17), false),
            "下一拍必须放行"
        );
    }

    /// 推迟 → 平台一次性唤醒 → 到期放行：状态不丢、只晚一拍。
    #[test]
    fn a_deferred_request_is_admitted_at_the_next_tick_and_clears_the_flag() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        budget.note_start(t0);
        assert!(!budget.admits(t0 + Duration::from_millis(1), false));
        assert!(budget.note_deferred(), "第一笔推迟必须让平台挂唤醒");
        assert!(budget.is_deferred());
        // Linux 的 calloop timeout 需要到期时刻；Windows/macOS 由节拍线程/定时器的节拍兜住。
        #[cfg(target_os = "linux")]
        {
            let deadline = budget.deadline().expect("推迟后必须有到期时刻");
            assert_eq!(deadline, t0 + Duration::from_secs_f64(1.0 / 60.0));
        }

        // 到期放行：推迟标记清掉，平台不再重复唤醒。
        budget.note_start(t0 + Duration::from_secs_f64(1.0 / 60.0));
        assert!(!budget.is_deferred());
        // 连续第二次推迟仍返回 true（每笔推迟都需要一次唤醒）。
        assert!(!budget.admits(
            t0 + Duration::from_secs_f64(1.0 / 60.0) + Duration::from_millis(1),
            false
        ));
        assert!(budget.note_deferred());
    }

    /// force 通道（resize / 拖动 / 动画 / 合成线程唤醒）不受预算约束，但仍然推进相位。
    #[test]
    fn the_force_channel_bypasses_the_budget_but_advances_the_phase() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        assert!(budget.admits(t0, true), "force 必须放行");
        budget.note_start(t0);
        assert!(
            budget.admits(t0 + Duration::from_millis(1), true),
            "force 在周期内也必须放行"
        );
        budget.note_start(t0 + Duration::from_millis(1));
        // force 帧推进了相位：普通请求跟着新的起点等一个周期。
        assert!(!budget.admits(t0 + Duration::from_millis(2), false));
        assert!(budget.admits(t0 + Duration::from_millis(18), false));
    }

    /// 未知 / 非法刷新率 ⇒ 门禁禁用（绝不用错误周期卡帧）；上下限边界必须精确。
    #[test]
    fn an_unknown_or_invalid_refresh_rate_disables_the_gate() {
        let mut budget = FrameBudget::default();
        let t0 = Instant::now();
        assert!(budget.admits(t0, false), "门禁未启用 ⇒ 永远放行");
        budget.note_start(t0);
        assert!(budget.admits(t0, false));

        for hz in [0.0, -60.0, f32::NAN, f32::INFINITY, 1.0, 1000.0] {
            budget.set_refresh_hz(hz);
            assert_eq!(budget.refresh_hz(), None, "非法提示 {hz} 必须禁用门禁");
            assert!(budget.admits(t0, false));
        }

        // 边界：24 / 480 合法；略越界（23.9 / 480.1）必须禁用。
        budget.set_refresh_hz(24.0);
        assert!(
            budget
                .refresh_hz()
                .is_some_and(|hz| (hz - 24.0).abs() < 0.01)
        );
        budget.set_refresh_hz(480.0);
        assert!(
            budget
                .refresh_hz()
                .is_some_and(|hz| (hz - 480.0).abs() < 0.01)
        );
        for hz in [23.9, 480.1] {
            budget.set_refresh_hz(hz);
            assert_eq!(budget.refresh_hz(), None, "边界外提示 {hz} 必须禁用门禁");
        }
    }

    /// 刷新率提示更新到合法值后，下一帧重新开始计时（换屏 / 显示恢复）。
    #[test]
    fn a_new_refresh_rate_restarts_the_phase() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        budget.note_start(t0);
        assert!(!budget.admits(t0 + Duration::from_millis(1), false));
        // 提示变化 ⇒ 相位重置：下一个请求立即放行（换屏后不能等旧周期）。
        budget.set_refresh_hz(120.0);
        assert!(budget.admits(t0, false), "提示变化必须重置相位并立即放行");
        assert!((budget.refresh_hz().expect("120 Hz") - 120.0).abs() < 0.01);
        budget.note_start(t0);
        // 120 Hz 周期 ≈ 8.33 ms：17 ms 后必然放行，5 ms 后仍在周期内。
        assert!(budget.admits(t0 + Duration::from_millis(17), false));
        assert!(!budget.admits(t0 + Duration::from_millis(5), false));
        // 同值写入幂等：不重置相位。
        budget.note_start(t0 + Duration::from_millis(20));
        budget.set_refresh_hz(120.0);
        assert!(
            !budget.admits(t0 + Duration::from_millis(21), false),
            "同值写入不得重置相位"
        );
        assert_eq!(budget.deferrals(), 0, "相位重置不清累计计数");
    }

    /// **RA-5 回归防线**：窗口不可见时清除「推迟未补」标记——只停一次性唤醒，不伪造放行。
    ///
    /// 回归：不可见窗口的平台帧循环会跳过它，`deferred` 永不清除 ⇒ mac 连续帧定时器常驻 /
    /// Linux calloop timeout=0 空转。
    #[test]
    fn clearing_the_deferred_flag_stops_the_one_shot_wakeup_without_forging_a_frame() {
        let mut budget = budget(60.0);
        let t0 = Instant::now();
        budget.note_start(t0);
        assert!(!budget.admits(t0 + Duration::from_millis(1), false));
        assert!(budget.note_deferred());
        assert!(budget.is_deferred());
        budget.clear_deferred();
        assert!(
            !budget.is_deferred(),
            "不可见窗口的推迟标记必须清掉（RA-5）"
        );
        assert!(
            !budget.admits(t0 + Duration::from_millis(2), false),
            "清除标记不等于放行（脏位保留、相位不变）"
        );
        assert_eq!(budget.deferrals(), 1, "累计计数保留（诊断量）");
    }
}
