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
    use super::{PresentedOrder, presentation_clock_supported};
    use crate::{
        render::present_evidence::{CLOCK_MONOTONIC, CLOCK_REALTIME},
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
}
