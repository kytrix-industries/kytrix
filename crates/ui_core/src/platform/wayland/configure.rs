//! xdg-shell configure 的 **ack 时序锁存**（B10）：把「收到即 ack」改为「使用新状态的帧
//! 发布之后再 ack」，并把初始 configure 的立即 ack 单列。
//!
//! # 问题
//!
//! 旧实现（`platform/linux/surface.rs` 的 `xdg_surface::Configure` 分支）收到
//! `xdg_surface.configure(serial)` 就**立刻** `ack_configure(serial)`，而 `xdg_toplevel` 的
//! 尺寸/状态在同一批事件里已经被写进 `WindowSurface` 并推了 `Resized`。协议原文
//! （`xdg-shell.xml`）把这个 serial 的语义说得很清楚：
//!
//! > Clients should arrange their surface for the new states, and then send an ack_configure
//! > request with the serial sent in this configure event at some point before committing the
//! > new surface.
//! >
//! > For instance, for toplevel surfaces the compositor might use this information to move a
//! > surface to the top left only when the client has drawn itself for the maximized or
//! > fullscreen state.
//!
//! 收到即 ack ⇒ compositor 认为「客户端已按新状态画好」，于是**先**切到最大化/全屏几何，而
//! 使用新尺寸的帧还在后面排队 —— 表现为 maximize / 全屏切换瞬间旧内容出现在新几何里
//! （审计 §10 已把报告「旧 buffer/destination 错位」的机制口径修正为这一点）。
//!
//! # 不变量（用户裁决 D7(a)，Qt `applyConfigureWhenPossible` 式）
//!
//! 1. **初始 configure 立即 ack**：协议要求 *"The client must acknowledge it and is then
//!    allowed to attach a buffer to map the surface."* —— 首帧映射不能等（不 ack 就不能 attach
//!    buffer，窗口永远不出现）；
//! 2. **后续尺寸/状态变化等「使用该状态的帧已发布」再 ack**（泵在 `App::frame` 返回
//!    `Committed` 后调用 [`ConfigureLatch::on_frame_published`] 取 serial）；
//! 3. **任意时刻最多一个未 ack 的 configure**：协议明确 *"If the client receives multiple
//!    configure events before it can respond to one, it only has to ack the last configure
//!    event."* ⇒ 后续 configure 覆盖 pending，只 ack 最后一条；
//! 4. **没有 buffer 状态变化**（焦点变化、wm_capabilities 等）的 configure 立即 ack：没有
//!    「新内容」需要等（协议 *"A client is not required to commit immediately after sending an
//!    ack_configure request"*）；
//! 5. **无法与帧配对**（窗口不可见 / 没有渲染器 / `Idle` 没有 publish）时由泵 force ack：
//!    静止窗口的 configure 若永远不确认，compositor 可能永远不完成该状态切换（例如映射
//!    期间的最小化/恢复），比「稍早 ack」更糟。
//!
//! # 为什么放在 `platform/wayland/`
//!
//! 与 B7/B8/B9 同一理由：`platform/linux/**` 只在 Linux 目标编译，开发机（Windows）的
//! `cargo test` 覆盖不到；锁存是纯状态机（输入 serial / 布尔，输出 [`AckAction`]），抽到这里
//! 后能在 Windows 单测里钉死上面五条不变量。整块 `platform::wayland` 在
//! `cfg(any(target_os = "linux", test))` 下编译。
//!
//! # Chrome / Qt 对照
//!
//! - Qt（`QWaylandWindow::applyConfigureWhenPossible` / `QWaylandXdgSurface`）：把 configure
//!   状态记成 pending，在「下一个可以换 buffer 尺寸的时机」应用并 ack；初始 configure 直接
//!   走 mapping 路径立即 ack。本锁存对应其 pending/apply 两段。
//! - Chromium（Ozone Wayland 的 `WaylandSurface` / `WaylandWindow`）：用 viz 的序列号把
//!   configure 与渲染帧配对；静态内容不需要重绘时立即 ack。本锁存用「帧已发布」作为配对点，
//!   没有严格到 GPU 线程的 `wl_surface.commit` 之前（审计 D7(b) 的严格配对改动更大，未采用）。
//!
//! # 已知残余
//!
//! - 帧由 GPU 线程 present 时 commit，UI 线程在 `Committed`（已发布给 GPU 线程）之后才排
//!   ack 请求；ack 与 GPU 线程 commit 的**线上顺序**取决于两个线程的 flush 时机，客户端无法
//!   保证 ack 一定在 commit 之前。语义上「新状态的内容已交给合成线程」已经成立，这正是不变量
//!   2 的「至少已发布」。真机 `WAYLAND_DEBUG=1` 观察到的顺序见手工步骤。
//! - **可见但被完全遮挡**（compositor 推迟 frame callback）时 pending 会随帧一起推迟；恢复
//!   可见后会补帧并 ack。这是帧门禁既有取舍的延伸（`wayland/frame_gate.rs` 文档），不会让
//!   窗口永久卡在未确认状态 —— 但若手工构造「永远不回来的 callback」则 ack 也永远不发。

/// `xdg_surface.ack_configure` 的发送时机（[`ConfigureLatch::on_surface_configure`] 的裁决）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AckAction {
    /// 立即 ack：初始 configure（协议要求先 ack 才能 attach buffer 映射），或本次 configure
    /// 没有需要重画的 buffer 状态变化。
    AckNow,
    /// 等使用该 configure 状态渲染的帧发布之后 ack（`on_frame_published` 取 serial）。
    Defer,
}

/// configure 锁存的阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// 还没收到首个 `xdg_surface.configure`。
    Initial,
    /// 已收到首个 configure 并 ack（surface 可以 map 了）。
    Mapped,
}

/// 一个窗口的 configure 锁存状态（纯逻辑，不持有 Wayland 代理）。
///
/// 生命周期与 `WindowSurface` 一致：窗口销毁时随协议对象一起 drop，未 ack 的 serial 不需要
/// （也**不能**）再 ack —— surface 对象已被销毁，ack 一个不存在的对象是协议错误。
pub(crate) struct ConfigureLatch {
    phase: Phase,
    /// 等待「使用其状态的帧已发布」的 configure serial；`None` = 没有待确认的 configure。
    pending: Option<u32>,
    /// 自上次 ack 以来，是否收到过**改变 buffer 状态**（尺寸 / 窗口 states）的
    /// `xdg_toplevel.configure`。wm_capabilities / configure_bounds / 装饰模式不属于此列。
    changed: bool,
}

impl ConfigureLatch {
    pub(crate) const fn new() -> Self {
        Self {
            phase: Phase::Initial,
            pending: None,
            changed: false,
        }
    }

    /// `xdg_toplevel.configure`：记录 buffer 状态（尺寸 / states）是否真的变化。
    ///
    /// 调用方负责比较：`buffer_state_changed = size != previous_size || states != previous_states`。
    /// `wm_capabilities` / `configure_bounds` **不**经过本入口（它们不改变需要重画的内容，
    /// 若走这里会让静止窗口为「能力通告」等一次永远不来的帧）。
    pub(crate) fn on_toplevel_configure(&mut self, buffer_state_changed: bool) {
        if buffer_state_changed {
            self.changed = true;
        }
    }

    /// `xdg_surface.configure(serial)`：裁决 ack 时机。
    ///
    /// - 初始 configure ⇒ [`AckAction::AckNow`] 并进入映射阶段（协议要求，见模块文档不变量 1）；
    /// - 有 buffer 状态变化 ⇒ [`AckAction::Defer`]，`pending = Some(serial)`（后到的 configure
    ///   覆盖前一个：只 ack 最后一条，见不变量 3）；
    /// - 没有变化 ⇒ [`AckAction::AckNow`]（没有「新内容」需要等）。
    pub(crate) fn on_surface_configure(&mut self, serial: u32) -> AckAction {
        if self.phase == Phase::Initial {
            // 初始 configure 可能同时带尺寸变化（compositor 常用于给出首个建议尺寸），但协议
            // 要求立即 ack 才能 attach buffer —— 初值不参与 defer 判定。
            self.phase = Phase::Mapped;
            self.changed = false;
            return AckAction::AckNow;
        }
        if self.changed {
            // 只留最后一条：更早的 serial 在 ack 最后一条时被协议自动消费
            // （*"Sending an ack_configure request consumes the serial number sent with the
            // request, as well as serial numbers sent by all configure events ... prior to
            // the configure event referenced by the committed serial."*）。
            self.pending = Some(serial);
            AckAction::Defer
        } else {
            AckAction::AckNow
        }
    }

    /// 使用该 configure 状态渲染的帧已发布（或 force 路径决定不再等帧）⇒ 取走待确认 serial。
    ///
    /// 返回 `None` = 没有 defer 过的 configure（无需 ack）。取走之后 `changed` 清零：下一次
    /// defer 必须由**新的** `xdg_toplevel.configure` 状态变化触发。
    ///
    /// **不清零** `changed` 的情况：`pending` 为空（例如 `xdg_toplevel.configure` 到了、而
    /// `xdg_surface.configure` 还没到就发生了一次帧发布）。这时若把 `changed` 清掉，后到的
    /// `xdg_surface.configure` 会被误判成「无变化」而立即 ack，回到修复前的时序。
    pub(crate) fn on_frame_published(&mut self) -> Option<u32> {
        let serial = self.pending.take()?;
        self.changed = false;
        Some(serial)
    }
}

#[cfg(test)]
mod tests {
    use super::{AckAction, ConfigureLatch};

    /// 建一个已经完成初始映射的锁存器（初始 configure 立即 ack）。
    fn mapped() -> ConfigureLatch {
        let mut latch = ConfigureLatch::new();
        assert_eq!(
            latch.on_surface_configure(1),
            AckAction::AckNow,
            "初始 configure 必须立即 ack（否则不能 attach buffer 映射）"
        );
        latch
    }

    /// B10 不变量 1：初始 configure 立即 ack，即使它同时带来尺寸变化；之后无变化的 configure
    /// 也立即 ack；没有 pending 时不会凭空 ack。
    ///
    /// 变异检查：把 Initial 分支也改成 `Defer`（= 「初始不 ack」）⇒ 本测试红。
    #[test]
    fn the_initial_configure_is_acked_immediately() {
        let mut latch = ConfigureLatch::new();
        // 初始 configure 通常同时给建议尺寸：仍必须立即 ack。
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(7), AckAction::AckNow);
        assert_eq!(
            latch.on_frame_published(),
            None,
            "初始 configure 已经 ack 过，不得再排队等帧"
        );

        // 映射之后、没有 buffer 状态变化 ⇒ 立即 ack（没有新内容要等）。
        assert_eq!(latch.on_surface_configure(8), AckAction::AckNow);
        assert_eq!(latch.on_frame_published(), None);
    }

    /// B10 不变量 2：buffer 状态变化 ⇒ Defer；帧发布后取到 serial，且只取一次。
    #[test]
    fn a_buffer_state_change_is_acked_after_the_frame_is_published() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(9), AckAction::Defer);
        assert_eq!(
            latch.on_frame_published(),
            Some(9),
            "帧发布之后才把 serial 交给 ack 路径"
        );
        assert_eq!(
            latch.on_frame_published(),
            None,
            "同一个 serial 只能 ack 一次"
        );
    }

    /// B10 不变量 3：帧发布之前连续收到多条 configure ⇒ 只 ack 最后一条（pending 覆盖）。
    ///
    /// 变异检查：把覆盖改成「保留第一条」（`pending.or(Some(serial))`）⇒ 本测试红。
    #[test]
    fn only_the_last_configure_is_acked() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(10), AckAction::Defer);
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(11), AckAction::Defer);
        assert_eq!(
            latch.pending,
            Some(11),
            "pending 只保留最后一条 configure 的 serial"
        );
        assert_eq!(latch.on_frame_published(), Some(11));
        assert_eq!(
            latch.on_frame_published(),
            None,
            "10 已被 11 隐式消费，不得再 ack"
        );
    }

    /// B10 不变量 4：没有 buffer 状态变化（例如只收到焦点 / 能力相关的 configure）⇒ 立即 ack，
    /// 不进入 defer。wm_capabilities / configure_bounds 根本不经过 `on_toplevel_configure`，
    /// 这里用 `false` 覆盖调用方传入的「未变化」。
    #[test]
    fn an_unchanged_configure_is_acked_immediately() {
        let mut latch = mapped();
        latch.on_toplevel_configure(false);
        assert_eq!(latch.on_surface_configure(12), AckAction::AckNow);
        assert_eq!(latch.on_frame_published(), None);

        // 前一条 defer 尚未发布时又来一条「无变化」的 configure：仍必须等前一条的状态帧，
        // pending 被新的 serial 覆盖（只 ack 最后一条）。
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(13), AckAction::Defer);
        assert_eq!(latch.on_surface_configure(14), AckAction::Defer);
        assert_eq!(latch.on_frame_published(), Some(14));
    }

    /// B10 不变量 2（序列）：Mapped 之后的 resize 序列 —— 每个「状态变化 + configure + 帧」
    /// 周期恰好 ack 一次，且 ack 的是**该周期最后一条** configure。
    #[test]
    fn a_resize_sequence_acks_once_per_published_frame() {
        let mut latch = mapped();
        let mut serial = 20;
        for _ in 0..4 {
            latch.on_toplevel_configure(true);
            assert_eq!(latch.on_surface_configure(serial), AckAction::Defer);
            assert_eq!(latch.on_frame_published(), Some(serial));
            serial += 1;
        }
        assert_eq!(latch.on_frame_published(), None);
        // 状态已经 ack 过 ⇒ 下一条无变化的 configure 立即 ack。
        assert_eq!(latch.on_surface_configure(serial), AckAction::AckNow);
    }

    /// B10 force 路径：窗口不可见 / Idle 时泵不等帧直接取走 pending（同一个
    /// `on_frame_published` 出口），并清掉 `changed` —— 之后无变化的 configure 不得再 defer。
    #[test]
    fn a_forced_ack_consumes_the_pending_serial() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(30), AckAction::Defer);
        assert_eq!(latch.on_frame_published(), Some(30), "force ack 走同一出口");
        assert_eq!(
            latch.on_surface_configure(31),
            AckAction::AckNow,
            "force ack 后 changed 清零：没有新状态变化就立即 ack"
        );
    }

    /// 空闲 / 不可见窗口不丢 serial：pending 在任意多轮「没有帧发布」的泵循环里保持不变，
    /// 直到 force ack 取走；期间与之无关的 toplevel 事件（未变化）不得清掉它。
    #[test]
    fn an_idle_window_keeps_its_pending_serial() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_surface_configure(40), AckAction::Defer);

        // 模拟多轮空闲泵：没有帧发布，只有无关的「未变化」事件。
        for _ in 0..3 {
            latch.on_toplevel_configure(false);
            assert_eq!(latch.pending, Some(40), "空闲期间 pending 不得丢失");
        }
        assert_eq!(latch.on_frame_published(), Some(40));
    }

    /// 半批事件保护：`xdg_toplevel.configure` 已到、`xdg_surface.configure` 还没到时就发生了
    /// 一次帧发布（另一窗口的帧 / 非配置帧）—— `changed` 不得被清掉，否则后到的
    /// `xdg_surface.configure` 会被误判为「无变化」而立即 ack（回到修复前时序）。
    #[test]
    fn a_frame_published_before_the_surface_configure_does_not_clear_the_change() {
        let mut latch = mapped();
        latch.on_toplevel_configure(true);
        assert_eq!(latch.on_frame_published(), None, "还没有 pending serial");
        assert_eq!(
            latch.on_surface_configure(50),
            AckAction::Defer,
            "changed 必须保留到该 configure 被 ack"
        );
        assert_eq!(latch.on_frame_published(), Some(50));
    }

    /// 多个窗口的锁存互不影响：A 窗口的 configure 不能被 B 窗口的帧发布 ack（泵按窗口下标
    /// 调用 `on_frame_published`，本测试只钉住锁存本身无全局状态）。
    #[test]
    fn latches_are_independent_per_window() {
        let mut first = mapped();
        let mut second = mapped();
        first.on_toplevel_configure(true);
        assert_eq!(first.on_surface_configure(60), AckAction::Defer);
        second.on_toplevel_configure(true);
        assert_eq!(second.on_surface_configure(61), AckAction::Defer);

        assert_eq!(second.on_frame_published(), Some(61));
        assert_eq!(
            first.pending,
            Some(60),
            "另一窗口的帧发布不得消费本窗口的 pending"
        );
        assert_eq!(first.on_frame_published(), Some(60));
    }
}
