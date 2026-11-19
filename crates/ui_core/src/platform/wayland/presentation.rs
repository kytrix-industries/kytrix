//! `wp_presentation` 呈现证据的**纯逻辑**（A4；开发机可单测）。
//!
//! Linux 的 `presented_at` 来自 Wayland 稳定的 presentation-time 扩展：GPU 线程在每次
//! `vkQueuePresentKHR`（= `wl_surface.attach/commit`）之前请求一个 `wp_presentation_feedback`
//! 对象，合成器在该内容真正上屏后回 `presented`（带呈现时钟时间戳）或 `discarded`。协议对象、
//! 时钟换算与事件接线分别在 `platform/linux/presentation.rs` / `platform/linux/surface.rs`；
//! 本模块只放协议无关、可在 Windows 开发机单测的判据：
//!
//! - [`presentation_clock_supported`]：compositor 宣告的 `clock_id` 是否是数据面支持的时钟域；
//! - [`PresentedOrder`]：presented 证据的**单调**判据（协议按提交顺序送呈现事件；乱序/陈旧
//!   事件说明反馈对象的帧归属出了偏差，只能丢弃，不能把证据记到错帧）。
//!
//! # 为什么帧归属不需要映射表
//!
//! 反馈请求由 **GPU 线程**在 present 之前发出（本仓唯一的 surface 提交者就是 `kytrix-vulkan`
//! 线程上的 Vulkan WSI），因此创建时 `FrameIndex` 已经确定，直接写进反馈对象的 user data
//! （`platform/linux/presentation.rs` 的 `FeedbackData`）。终态事件到达时按 user data 取帧即可，
//! 不存在「先武装、后补帧序号」的窗口期。
//!
//! **不能**把请求放在 UI 线程的「发布之后」：提交由 `App::frame` 发起，而 `App::frame` 既可能
//! 由平台泵调用、也可能被应用的帧钩子（`App::on_frame`，如 `acceptance window`）直接调用；
//! UI 线程没有任何位置能保证「请求先于该帧的 commit」入线（GPU 线程随时可能已经提交），
//! 反馈对象会绑到下一次提交并把证据记到错帧。GPU 线程在 present 前请求是唯一无竞争的位置。

use std::sync::atomic::{AtomicBool, Ordering};

use crate::{
    render::present_evidence::{CLOCK_MONOTONIC, CLOCK_REALTIME},
    window::FrameIndex,
};

/// compositor 宣告的 `wp_presentation.clock_id` 是否是数据面支持的时钟域。
///
/// 数据面只接受 POSIX `CLOCK_REALTIME` / `CLOCK_MONOTONIC`（sway / Mutter / KWin 都发
/// `CLOCK_MONOTONIC`）：只有它们能用「同一时钟域的 `clock_gettime` 读数 + 同刻 `Instant`」
/// 做线性锚点映射。`CLOCK_MONOTONIC_RAW` / `CLOCK_BOOTTIME` 等与 `Instant` 的时基关系没有
/// 保证 ⇒ 拒绝（[`crate::render::present_evidence::presented_event_instant`] 同样只认这两个；
/// 这里提前判是为了不拿不支持的时钟域去调 `clock_gettime`，也便于单测拒绝路径）。
pub(crate) fn presentation_clock_supported(clock_id: i32) -> bool {
    matches!(clock_id, CLOCK_REALTIME | CLOCK_MONOTONIC)
}

/// 呈现实证句柄的**连接有效性**（RB-05）。
///
/// `wp_presentation` 是连接级单例：GPU 线程持有的 [`PresentationHandle`] 里只保存它的代理
/// `Clone`。若 compositor 在运行期移除该 global，协议要求客户端销毁绑定对象；销毁后旧代理
/// 会变成 inert proxy —— 继续 `feedback()` 是**静默 no-op**，该窗口的 `presented_at` 会永久
/// `None` 且没有任何诊断。本类型把「是否仍有效」做成共享原子标志：
///
/// - UI 线程在 `global_remove` 时 [`Self::invalidate`]（返回是否首次失效，用于一次性诊断）；
/// - GPU 线程每次请求前 [`Self::is_valid`] 短路，不再对已销毁代理发请求；
/// - 已下发的句柄**严格失效**（不重建 renderer 就保持 None，契约如实、不伪造）；运行期重新
///   宣告 global 时新 renderer 会拿到新的有效期。
#[derive(Debug)]
pub(crate) struct PresentationValidity {
    valid: AtomicBool,
}

impl PresentationValidity {
    pub(crate) const fn new() -> Self {
        Self {
            valid: AtomicBool::new(true),
        }
    }

    /// 标记失效；返回 `true` = 本次是首次失效（诊断只打一次）。
    pub(crate) fn invalidate(&self) -> bool {
        self.valid.swap(false, Ordering::Release)
    }

    /// 是否仍可请求呈现反馈（GPU 线程读）。
    pub(crate) fn is_valid(&self) -> bool {
        self.valid.load(Ordering::Acquire)
    }
}

impl Default for PresentationValidity {
    fn default() -> Self {
        Self::new()
    }
}

/// 共享有效期（`PresentationHandle` 与 [`crate::platform::linux::presentation::Presentation`]
/// 之间共享）。用独立类型别名而不是裸 `Arc` 便于阅读；只有 Linux 生产路径消费它（其余平台/
/// 测试构建不编译 Linux 模块，别名按目标门控避免死代码）。
#[cfg(target_os = "linux")]
pub(crate) type SharedPresentationValidity = std::sync::Arc<PresentationValidity>;

/// presented 证据的单调判据（每窗口一份）。
///
/// 协议保证一条反馈恰好一个终态，且呈现事件按「提交顺序」送达（同一个连接上的事件保序）。
/// 因此一个窗口的 `presented` 帧序号必须**严格递增**：不递增只可能来自「反馈对象绑到了
/// 比它声明的帧更晚的提交」这类归属偏差（例如 present 失败后待画帧又被更新的帧吞并），
/// 这种证据宁可丢弃（该帧如实 `None`）也不能记到错帧。
pub(crate) struct PresentedOrder {
    /// 已接受的最大 presented 帧序号（`None` = 还没有接受过）。
    last: Option<FrameIndex>,
}

impl PresentedOrder {
    pub(crate) const fn new() -> Self {
        Self { last: None }
    }

    /// 该 presented 证据是否可信（帧序号严格递增）。接受时推进 watermark。
    pub(crate) fn accept(&mut self, frame: FrameIndex) -> bool {
        if self.last.is_some_and(|last| frame.get() <= last.get()) {
            return false;
        }
        self.last = Some(frame);
        true
    }

    /// 当前 watermark（测试断言面；生产只经 [`Self::accept`] 推进）。
    #[cfg(test)]
    pub(crate) const fn last(&self) -> Option<FrameIndex> {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{PresentationValidity, PresentedOrder, presentation_clock_supported};
    use crate::{
        render::present_evidence::{CLOCK_MONOTONIC, CLOCK_REALTIME, presented_event_instant},
        window::FrameIndex,
    };

    fn frame(value: u32) -> FrameIndex {
        FrameIndex::from_raw(value)
    }

    /// 只有 `CLOCK_REALTIME` / `CLOCK_MONOTONIC` 是可信呈现时钟；其余时钟域（`MONOTONIC_RAW` /
    /// `BOOTTIME` / CPU 时钟 …）一律拒绝，绝不拿与 `Instant` 无保证关系的时钟域换算。
    #[test]
    fn only_real_time_and_monotonic_are_supported_presentation_clocks() {
        assert!(presentation_clock_supported(CLOCK_REALTIME));
        assert!(presentation_clock_supported(CLOCK_MONOTONIC));
        for clock in [2, 3, 4, 5, 6, 7, -1] {
            assert!(
                !presentation_clock_supported(clock),
                "clock_id={clock} 不是数据面支持的呈现时钟"
            );
        }
    }

    /// 呈现证据按提交顺序递增：乱序（后帧先到）/ 重复的 presented 只能丢弃，不能覆盖或
    /// 回退 watermark（变异「接受所有事件」会让本用例红）。
    #[test]
    fn presented_evidence_must_arrive_in_submission_order() {
        let mut order = PresentedOrder::new();
        assert!(order.accept(frame(3)), "首个 presented 证据");
        assert!(order.accept(frame(5)), "跳号（中间帧没提交）仍递增");
        assert!(!order.accept(frame(4)), "旧帧迟到 ⇒ 丢弃");
        assert!(!order.accept(frame(5)), "重复事件 ⇒ 丢弃");
        assert_eq!(order.last(), Some(frame(5)), "watermark 只前进");
        assert!(order.accept(frame(6)));
        assert_eq!(order.last(), Some(frame(6)));
    }

    /// RB-05：`wp_presentation` 在运行期被移除后，已下发的句柄严格失效（首次失效返回 true
    /// 供一次性诊断，之后 idempotent；GPU 线程读 `is_valid()==false` 后不再请求）。
    #[test]
    fn a_removed_presentation_global_strictly_invalidates_existing_handles() {
        let validity = PresentationValidity::new();
        assert!(validity.is_valid(), "初始有效");
        assert!(validity.invalidate(), "首次失效用于一次性诊断");
        assert!(!validity.is_valid(), "失效后句柄不得再请求呈现反馈");
        assert!(!validity.invalidate(), "重复失效幂等（不重复诊断）");
    }

    /// RB-04：映射方向必须是**下界**。
    ///
    /// 生产顺序是「先取 `Instant::now()` 作锚点，再读同一时钟域的 `clock_gettime`」，因此读到的
    /// 时钟值对应锚点**之后**的时刻（这里用事件 ts 1000.0 s、锚点后 0.02 s 才读到时钟 =
    /// 1000.02 s 模拟）。换算式 `anchor + (presented - clock_anchor)` 得到
    /// `anchor - 0.02 s`：严格早于锚点，即不晚于真实上屏时刻（下界，偏差方向恒为偏早）。
    ///
    /// 变异检查：把锚点顺序换回「先读时钟、后取 `Instant`」（对应对拍里 `clock_anchor` 比
    /// 锚点时刻**小** gap），结果会变成 `anchor + gap` ⇒ 本测试红。
    #[test]
    fn a_clock_anchor_read_after_the_instant_maps_presented_strictly_early() {
        let anchor = Instant::now();
        let clock_at_anchor = 1000.0_f64;
        let clock_anchor = clock_at_anchor + 0.02;
        let mapped = presented_event_instant(
            0,
            1000,
            0,
            CLOCK_MONOTONIC,
            clock_anchor,
            anchor,
            anchor + Duration::from_secs(1),
        )
        .expect("偏差 20ms 在可信范围内");
        assert!(
            mapped < anchor,
            "presented_at 必须是真实上屏时刻的下界（不得晚于锚点）"
        );
        assert_eq!(
            mapped,
            anchor
                .checked_sub(Duration::from_secs_f64(0.02))
                .expect("Instant 减法")
        );
    }
}
