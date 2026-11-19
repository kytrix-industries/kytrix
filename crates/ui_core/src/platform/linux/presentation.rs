//! `wp_presentation`（Wayland 稳定扩展 presentation-time）的绑定与呈现证据换算（A4）。
//!
//! 职责边界：本文件只做**协议对象 + 时钟域**这一层薄壳 ——
//!
//! - `WaylandState` 在 registry 里看到 `wp_presentation` 时调用 [`Presentation::bind`]；
//! - 建渲染器时用 [`Presentation::handle`] 取一个 [`PresentationHandle`] 交给 GPU 线程；
//!   GPU 线程在**每次 present 之前**（`render/linux/renderer.rs` 的 `submit_frame` 调用点）
//!   调 [`PresentationHandle::request_feedback`]，把 `(WindowId, FrameIndex)` 写进反馈对象的
//!   user data；
//! - 终态事件（`presented` / `discarded`）到达时调用 [`Presentation::presented_instant`]：
//!   按 compositor 宣告的呈现时钟域取同刻锚点，经
//!   [`presented_event_instant`](crate::render::present_evidence::presented_event_instant)
//!   换算成 `Instant`（不可信就返回 `None` 并记豁免，**绝不伪造**）。
//!
//! 事件派发 → 邮箱 → 泵回填的接线在 `platform/linux/surface.rs` 与 `event_loop.rs`；可单测的
//! 判据（时钟域白名单 / presented 证据单调性）在 `platform/wayland/presentation.rs`。
//!
//! # 为什么在 GPU 线程请求 feedback
//!
//! 反馈对象绑定的是「该 surface 的**下一次** `wl_surface.commit`」。本仓唯一的 surface 提交者
//! 是 `kytrix-vulkan` 线程上的 Vulkan WSI（`vkQueuePresentKHR` → Wayland `attach/commit`），
//! 因此 GPU 线程在 present 前请求就是「恰好在这一次 commit 之前」：请求先入 libwayland 的
//! 发送缓冲，Mesa 随后的 attach/commit 排在它后面（同一 `wl_display` 的发送缓冲由 libwayland
//! 互斥锁保证入队顺序）。创建时帧序号已确定 ⇒ 不需要任何「后补帧序号」的映射表。
//!
//! UI 线程**没有**等价的安全点：提交由 `App::frame` 发起，而它既可能由平台泵调用、也可能被
//! 应用的帧钩子（`App::on_frame`，如 `acceptance window`、`demo`）直接调用；UI 线程无法保证
//! 「请求先于该帧 commit 入线」（GPU 线程随时可能已经提交下去），反馈对象会绑到下一次提交并
//! 把证据记到错帧。详见 `platform/wayland/presentation.rs` 的模块文档。
//!
//! # 为什么用 libc 的 `clock_gettime`
//!
//! `wp_presentation.clock_id` 是 POSIX `clockid_t`（协议原文："the identifier value is one of
//! the clockid_t values accepted by clock_gettime()"）。要把它换算到 Rust 的 `Instant`，必须
//! 在**同一时钟域**取一个锚点读数（秒）并立刻取 `Instant::now()`；`std` 不暴露 `Instant` 的
//! 秒读数，只能调 `clock_gettime`。`libc` 已在 Cargo.lock（传递依赖），本次只把它提为 Linux
//! 目标的直接依赖，且**只**用于这一个系统调用（Lead 裁决的 A 选项）。
//!
//! # 豁免口径（审计 Q6-L1）
//!
//! compositor 没有 `wp_presentation`（或时钟域不可用 / 时间戳被判据拒绝）时，`presented_at`
//! 如实保持 `None` 并计数 + 一次性诊断；不拿 `completed_at` 或别的时刻顶替。

use std::time::Instant;

use wayland_client::{
    QueueHandle,
    protocol::{wl_registry, wl_surface},
};
use wayland_protocols::wp::presentation_time::client::wp_presentation::WpPresentation;

use super::surface::WaylandState;
use crate::{
    render::present_evidence::presented_event_instant,
    window::{FrameIndex, WindowId},
};

/// 一个 `wp_presentation_feedback` 对象的 user data：终态事件到达时指出「哪个窗口、哪一帧」。
///
/// 反馈对象由 GPU 线程在 present 之前创建，此时 `FrameIndex` 已确定（`FrameRequest.index`）；
/// 事件在 UI 线程派发，按本结构取帧即可。`Send + Sync + 'static` 由 wayland-client 的
/// user data 契约要求。字段私有（workspace 禁止字段级作用域可见性）：跨模块读取走
/// [`Self::window`] / [`Self::frame`]。
#[derive(Clone, Copy, Debug)]
pub(crate) struct FeedbackData {
    window: WindowId,
    frame: FrameIndex,
}

impl FeedbackData {
    pub(crate) const fn new(window: WindowId, frame: FrameIndex) -> Self {
        Self { window, frame }
    }

    /// 该呈现事件属于哪个窗口（终态派发按它路由到窗口的呈现邮箱）。
    pub(crate) const fn window(self) -> WindowId {
        self.window
    }

    /// 该呈现事件属于哪一帧（创建反馈对象时由 GPU 线程写入）。
    pub(crate) const fn frame(self) -> FrameIndex {
        self.frame
    }
}

/// 呈现证据「拿不到」的分类（A4 豁免口径）。
#[derive(Clone, Copy, Debug)]
pub(crate) enum Exemption {
    /// compositor 没有 `wp_presentation` 全局（或 global 绑定失败）。
    ProtocolMissing,
    /// `clock_gettime(clock_id)` 失败（声称的时钟域不可用）。
    ClockUnavailable,
    /// 终态事件的呈现时钟不可信：时钟域不支持 / `tv_nsec` 越界 / 与锚点偏差超限 /
    /// 时间戳晚于采样时刻（数据面判据拒绝）。
    Rejected,
    /// presented 事件的帧归属/顺序不可信（非递增，见
    /// [`PresentedOrder`](crate::platform::wayland::presentation::PresentedOrder)）。
    Unmatched,
}

/// 各类豁免的计数 + 一次性诊断。
///
/// 诊断只打**第一次**（协议缺失时每个窗口建渲染器都会走到这里，不刷屏），计数保留到连接
/// 结束供人工核查；正常合成器（sway / Mutter / KWin）恒为 0。
#[derive(Debug, Default)]
pub(crate) struct Exemptions {
    protocol_missing: u64,
    clock_unavailable: u64,
    rejected: u64,
    unmatched: u64,
}

impl Exemptions {
    /// 记一次豁免；该类第一次出现时输出一条诊断。
    pub(crate) fn note(&mut self, kind: Exemption) {
        let count = match kind {
            Exemption::ProtocolMissing => &mut self.protocol_missing,
            Exemption::ClockUnavailable => &mut self.clock_unavailable,
            Exemption::Rejected => &mut self.rejected,
            Exemption::Unmatched => &mut self.unmatched,
        };
        *count = count.saturating_add(1);
        if *count == 1 {
            match kind {
                Exemption::ProtocolMissing => eprintln!(
                    "kytrix: compositor has no wp_presentation; presented_at stays None on this \
                     connection (exemption counted, no fabricated timestamps)"
                ),
                Exemption::ClockUnavailable => eprintln!(
                    "kytrix: clock_gettime for the presentation clock failed; presented_at stays \
                     None for the affected frames (exemption counted)"
                ),
                Exemption::Rejected => eprintln!(
                    "kytrix: wp_presentation timestamp rejected by the evidence contract \
                     (unsupported clock domain / invalid nsec / implausible delta); \
                     presented_at stays None for the affected frames"
                ),
                Exemption::Unmatched => eprintln!(
                    "kytrix: wp_presentation presented event is out of submission order \
                     (stale binding); evidence dropped for that frame"
                ),
            }
        }
    }
}

/// 连接级的 `wp_presentation` 绑定状态（每连接一份）。
///
/// `clock_id` 单独保存在绑定之外：compositor 在 bind 之后立刻发一次 `clock_id`，此后连接
/// 生命周期内不变（协议原文："The presentation clock does not change during the lifetime of
/// the client connection"）；即使 global 被移除，已经发出的 feedback 对象仍会用同一时钟域
/// 送终态，换算仍需要它。
pub(crate) struct Presentation {
    /// 绑定的 `wp_presentation`（`None` = compositor 不支持 / 尚未绑定）。
    proxy: Option<WpPresentation>,
    /// registry global name（`global_remove` 的匹配键）。
    global_name: Option<u32>,
    /// compositor 宣告的呈现时钟域（`wp_presentation.clock_id`；`None` = 还没收到）。
    clock_id: Option<i32>,
    /// 豁免计数 / 一次性诊断。
    exemptions: Exemptions,
}

impl Presentation {
    pub(crate) fn new() -> Self {
        Self {
            proxy: None,
            global_name: None,
            clock_id: None,
            exemptions: Exemptions::default(),
        }
    }

    /// 绑定一个 `wp_presentation` global（幂等；已绑定则忽略）。
    ///
    /// 只绑 v1：本模块只用 `clock_id` 与 `feedback` / `presented` / `discarded`，v2 对
    /// 「非常量刷新率」的 `refresh` 语义调整不影响它们；绑低版本不额外要求 compositor 版本。
    pub(crate) fn bind(
        &mut self,
        registry: &wl_registry::WlRegistry,
        queue: &QueueHandle<WaylandState>,
        name: u32,
        version: u32,
    ) {
        if self.proxy.is_some() || version == 0 {
            return;
        }
        let proxy = registry.bind::<WpPresentation, _, _>(name, version.min(1), queue, ());
        self.proxy = Some(proxy);
        self.global_name = Some(name);
    }

    /// `wl_registry.global_remove`：销毁绑定对象（协议要求客户端随后销毁它）。
    ///
    /// 已创建的 feedback 对象不受影响（`wp_presentation.destroy` 的协议原文："Existing
    /// objects created by this object are not affected"），它们的终态事件仍会到达并参与换算。
    pub(crate) fn unbind(&mut self, name: u32) {
        if self.global_name != Some(name) {
            return;
        }
        if let Some(proxy) = self.proxy.take() {
            proxy.destroy();
        }
        self.global_name = None;
    }

    /// `wp_presentation.clock_id`：记录 compositor 的呈现时钟域。
    pub(crate) fn note_clock_id(&mut self, clock_id: u32) {
        // POSIX `clockid_t` 是 `i32`；协议字段是 `uint`。`CLOCK_*` 都是小正数，越界值直接丢弃
        // （换算时按「没有宣告」处理 ⇒ 如实 None）。
        self.clock_id = i32::try_from(clock_id).ok();
    }

    /// 取一个可移到 GPU 线程的反馈请求句柄（协议不可用时返回 `None` 并记豁免）。
    ///
    /// 句柄持有 `wp_presentation` 代理、事件队列句柄与该窗口的 `wl_surface` 副本：GPU 线程在
    /// present 前直接创建 feedback 对象，事件仍由 UI 线程的默认队列派发。
    pub(crate) fn handle(
        &mut self,
        window: WindowId,
        queue: &QueueHandle<WaylandState>,
        surface: wl_surface::WlSurface,
    ) -> Option<PresentationHandle> {
        let Some(proxy) = self.proxy.clone() else {
            self.exemptions.note(Exemption::ProtocolMissing);
            return None;
        };
        Some(PresentationHandle {
            proxy,
            queue: queue.clone(),
            window,
            surface,
        })
    }

    /// `presented` 事件 → 可信的呈现时刻（`None` = 该帧没有可信值，已记豁免）。
    ///
    /// 锚点取法：先 `clock_gettime(clock_id)` 读秒，再**立刻** `Instant::now()`；呈现时间戳是
    /// 事件生成时刻（早于我们读到它），所以换算结果不会晚于锚点。1 s 的合理距离判据由
    /// [`presented_event_instant`] 负责（`tv_nsec` 越界 / 未知时钟域 / 偏差超限 ⇒ `None`）。
    pub(crate) fn presented_instant(
        &mut self,
        tv_sec_hi: u32,
        tv_sec_lo: u32,
        tv_nsec: u32,
    ) -> Option<Instant> {
        let Some(clock_id) = self.clock_id else {
            // 还没收到 `clock_id`：无法把 compositor 的时间戳搬到任何本地时基。
            self.exemptions.note(Exemption::Rejected);
            return None;
        };
        if !crate::platform::wayland::presentation::presentation_clock_supported(clock_id) {
            self.exemptions.note(Exemption::Rejected);
            return None;
        }
        let Some(clock_secs) = clock_seconds(clock_id) else {
            self.exemptions.note(Exemption::ClockUnavailable);
            return None;
        };
        let anchor = Instant::now();
        let instant = presented_event_instant(
            tv_sec_hi, tv_sec_lo, tv_nsec, clock_id, clock_secs, anchor, anchor,
        );
        if instant.is_none() {
            self.exemptions.note(Exemption::Rejected);
        }
        instant
    }

    /// 记一次「presented 证据帧归属/顺序不可信」（`surface.rs` 的事件派发用）。
    pub(crate) fn note_unmatched_feedback(&mut self) {
        self.exemptions.note(Exemption::Unmatched);
    }
}

/// GPU 线程持有的呈现反馈请求句柄（每个窗口一份）。
///
/// 只暴露一件事：[`Self::request_feedback`]。句柄里保存 `wl_surface` 的 `Clone` 代理（与
/// 渲染器自己持有的那份同源），因此 GPU 线程请求 feedback 不需要回 UI 线程借协议对象。
pub(crate) struct PresentationHandle {
    proxy: WpPresentation,
    queue: QueueHandle<WaylandState>,
    window: WindowId,
    surface: wl_surface::WlSurface,
}

impl PresentationHandle {
    /// 为 `frame` 请求一次呈现反馈（**必须在 present 之前调用**；见模块文档）。
    ///
    /// 幂等由调用方按帧序号去重（重试同一帧不能重复创建对象，否则一次 commit 产生多条反馈）。
    /// 返回的代理句柄不需要保留：事件由连接的对象表按事件队列派发，`presented` / `discarded`
    /// 都是 destructor 事件，终态到达后对象自动销毁。
    pub(crate) fn request_feedback(&self, frame: FrameIndex) {
        let _feedback = self.proxy.feedback(
            &self.surface,
            &self.queue,
            FeedbackData::new(self.window, frame),
        );
    }
}

/// `clock_gettime(clock_id)` 的秒读数（f64）。
///
/// 只在 compositor 宣告的呈现时钟域上调用；失败或纳秒字段越界返回 `None`（调方按豁免处理）。
/// `tv_sec` 装进 f64 的精度损失在 Unix 纪元量级（~1.7e9 s）远小于 1 ns 的 2^-52 相对精度，
/// 对「呈现时间戳与锚点的差值」没有可测影响。
fn clock_seconds(clock_id: i32) -> Option<f64> {
    // SAFETY: `libc::timespec` 的字段布局与对齐由 libc 保证（`time_t` / `c_long`），
    // `clock_gettime` 只写入该结构、不保留指针；`clock_id` 已先经数据面判据过滤
    // （Linux 的 `libc::clockid_t` 是 `i32`，与协议字段转换后的类型一致）。
    let mut timespec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let result = unsafe { libc::clock_gettime(clock_id, &mut timespec) };
    if result != 0 {
        return None;
    }
    // 合法值的纳秒部分恒在 [0, 1e9)；越界说明系统调用返回了垃圾值（或结构布局不符）。
    if !(0..1_000_000_000).contains(&timespec.tv_nsec) {
        return None;
    }
    Some(timespec.tv_sec as f64 + timespec.tv_nsec as f64 / 1e9)
}

#[cfg(test)]
mod tests {
    use super::{Exemption, Exemptions, clock_seconds};
    use crate::render::present_evidence::{CLOCK_MONOTONIC, CLOCK_REALTIME};

    /// `clock_seconds` 是 libc FFI 的薄壳：结构布局正确时读数必须单调前进且在合理量级。
    /// （Linux 目标才有本测试 —— 开发机是 Windows，远端 `cargo test` 覆盖。）
    #[test]
    fn the_presentation_clock_is_read_through_libc() {
        let first = clock_seconds(CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC 必须可用");
        let second = clock_seconds(CLOCK_MONOTONIC).expect("CLOCK_MONOTONIC 必须可用");
        assert!(first.is_finite(), "读数必须是有限值");
        assert!(second >= first, "单调时钟不得回退");
        assert!(
            first > 0.0,
            "单调时钟自开机起计数 ⇒ 读数应为正（布局/字段错位会读出 0）"
        );
        // CLOCK_REALTIME 是 Unix 纪元秒，2020 年以后必然 > 1.5e9。
        let realtime = clock_seconds(CLOCK_REALTIME).expect("CLOCK_REALTIME 必须可用");
        assert!(realtime > 1.5e9, "REALTIME 读数应在 Unix 纪元量级");
        // 不支持的时钟域（负 / 超出 POSIX clockid_t 有效区间）由内核拒绝 ⇒ None。
        assert!(clock_seconds(-1).is_none(), "非法 clockid 必须失败");
    }

    /// 豁免诊断只计数、不伪造：同一类重复记也只增加计数（诊断输出的一次性由 `note` 内部保证）。
    #[test]
    fn exemption_counters_are_monotonic_and_do_not_forge_evidence() {
        let mut exemptions = Exemptions::default();
        exemptions.note(Exemption::ProtocolMissing);
        exemptions.note(Exemption::ProtocolMissing);
        exemptions.note(Exemption::Rejected);
        let debug = format!("{exemptions:?}");
        assert!(
            debug.contains("protocol_missing: 2"),
            "同类重复计数: {debug}"
        );
        assert!(debug.contains("rejected: 1"), "分类计数: {debug}");
        assert!(debug.contains("clock_unavailable: 0"), "未发生的类保持 0");
    }
}
