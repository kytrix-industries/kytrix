//! macOS 关闭语义的**平台无关状态机**（B5）。
//!
//! 两个关闭来源必须严格区分：
//!
//! - **用户手势**（红点 / `Cmd+W` / 菜单 Close / `performClose:`；以及防御路径
//!   `windowShouldClose:`）：可否决 —— 引擎先投递 `Event::CloseRequested`，再由
//!   `App::request_close` 跑应用注册的关闭处理器；否决则窗口保留。
//! - **外部关闭**（`windowWillClose:`）：不可否决 —— AppKit 已经关闭了窗口，引擎只做
//!   App 侧关闭记账并移除原生窗口，**不再投递事件**。
//!
//! 两个来源若各用一个 `Cell<bool>` 表达，泵就无法回答「先处理哪个」：一个尚未裁决的手势
//! 请求与一个已经发生的 `windowWillClose:` 同时待处理时，后者必须**压过**前者（窗口已经
//! 没了，再投递 `CloseRequested` / 再询问 `on_close` 都是对不存在的窗口做事）。因此这里用
//! 一个三态状态机：
//!
//! ```text
//!   Idle ──request()──▶ Requested ──will_close()──▶ WillClose
//!     ▲                    │                          │
//!     │◀── decide(veto) ───┘                          │
//!     │◀────────────── decide(allow) ─────────────────┘
//!     └────────────────────────────────────────────────┘
//! ```
//!
//! - `decide` 对 `Requested` 调一次裁决闭包（严格一次；重复 `request()` 在 `Requested`
//!   状态下返回 `false`，不会产生第二条事件）；
//! - `decide` 对 `WillClose` **不调**裁决闭包（没有可裁决的对象）；`WillClose` 优先于
//!   `Requested`，因为外部关闭是终态。
//!
//! # 为什么在 `platform/` 根而不是 `platform/macos/` 下
//!
//! `platform::macos` 整个模块只在 macOS 目标编译（objc2 依赖也是 macOS-only），而这里的
//! 状态迁移与 AppKit 类型无关，必须能在开发机（Windows）单测 —— 与 [`crate::platform::menu`]、
//! [`crate::platform::macos_placement`] 同一模式，不使用 `#[path]`。
//! ObjC 侧只负责置位（`GuiView::performClose:` / `WindowDelegate::windowShouldClose:` /
//! `WindowDelegate::windowWillClose:`）与唤醒泵；泵每轮调 [`CloseBridge::decide`]。

use std::cell::Cell;

/// 关闭桥的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseState {
    /// 没有待处理的关闭。
    Idle,
    /// 用户手势已到达、等泵裁决（可否决）。
    Requested,
    /// `windowWillClose:` 已到达：窗口已经被 AppKit 关闭（不可否决、终态）。
    WillClose,
}

/// 关闭桥：`GuiView`（`performClose:` / `windowShouldClose:`）与 `WindowDelegate`
/// （`windowWillClose:`）共享，泵每轮消费。
///
/// 只在主线程使用（`Cell` 非 `Send`，ObjC 回调与泵都在主线程）；`Rc` 共享即可，不需要
/// 跨线程同步。
#[derive(Debug)]
pub(crate) struct CloseBridge {
    state: Cell<CloseState>,
}

/// 泵对一扇窗口的关闭决议。
///
/// 泵据此执行平台动作；决议本身不包含「怎么关」的细节（`App::request_close` 已在裁决闭包
/// 内关闭了 App 窗口，原生窗口由泵关闭）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseAction {
    /// 没有关闭请求，或被应用否决：窗口保留，不产生任何动作。
    Keep,
    /// 用户手势被批准：裁决闭包已经关掉 App 窗口，泵关闭原生窗口。
    Close,
    /// 外部关闭：AppKit 已关闭窗口，泵只做 App 侧记账并移除原生窗口（不投递事件）。
    RemoveNative,
}

impl CloseBridge {
    pub(crate) const fn new() -> Self {
        Self {
            state: Cell::new(CloseState::Idle),
        }
    }

    /// 用户关窗手势到达（`performClose:` / `windowShouldClose:`）。
    ///
    /// 返回 `true` = **新**请求（调用方需要 `dispatcher.wake()`）：重复手势（AppKit 在一次
    /// 关闭动作里既调 view 的 `performClose:` 又走 delegate 等）只置位一次，泵因此只投递
    /// 一条 `CloseRequested`、只问一次 `on_close`。`WillClose` 之后的新手势同样被忽略 ——
    /// 窗口已经在关闭流程里。
    pub(crate) fn request(&self) -> bool {
        if self.state.get() != CloseState::Idle {
            return false;
        }
        self.state.set(CloseState::Requested);
        true
    }

    /// `windowWillClose:` 到达：外部关闭是终态，压过尚未裁决的手势请求。
    ///
    /// 返回 `true` = 第一次收到（调用方需要 `dispatcher.wake()`）。
    pub(crate) fn will_close(&self) -> bool {
        if self.state.get() == CloseState::WillClose {
            return false;
        }
        self.state.set(CloseState::WillClose);
        true
    }

    /// 泵每轮对一扇窗口的关闭决议。
    ///
    /// `verdict` 只在**用户手势**路径被调用，且恰好一次；调用方在其中先
    /// `App::dispatch(Event::CloseRequested)` 再返回 `App::request_close` 的裁决 ——
    /// 事件先于关闭处理器（与 Windows 同序）。外部关闭路径不调用它（窗口已经没了）。
    pub(crate) fn decide(&self, verdict: impl FnOnce() -> bool) -> CloseAction {
        match self.state.get() {
            // 外部关闭压过尚未裁决的手势：窗口已经关了，不再投递事件/询问裁决。
            CloseState::WillClose => {
                self.state.set(CloseState::Idle);
                CloseAction::RemoveNative
            }
            CloseState::Requested => {
                self.state.set(CloseState::Idle);
                if verdict() {
                    CloseAction::Close
                } else {
                    CloseAction::Keep
                }
            }
            CloseState::Idle => CloseAction::Keep,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CloseAction, CloseBridge};

    /// 用户手势在桥上去重：两次 `performClose:` 只产生一次事件/一次裁决；批准后关闭一次。
    #[test]
    fn a_gesture_is_delivered_and_decided_exactly_once() {
        let bridge = CloseBridge::new();
        assert!(bridge.request());
        assert!(!bridge.request(), "重复 performClose 必须在桥上去重");
        let mut verdicts = 0_u8;
        let action = bridge.decide(|| {
            verdicts += 1;
            true
        });
        assert_eq!(action, CloseAction::Close);
        assert_eq!(verdicts, 1, "一次手势只能裁决一次");
        assert_eq!(
            bridge.decide(|| panic!("没有新请求时不得再次询问裁决")),
            CloseAction::Keep
        );
    }

    /// 否决：窗口保留、桥回到空闲；不进入 `windowWillClose:` 路径；下一次手势重新可用。
    #[test]
    fn a_veto_keeps_the_window_and_returns_to_idle() {
        let bridge = CloseBridge::new();
        assert!(bridge.request());
        assert_eq!(bridge.decide(|| false), CloseAction::Keep);
        // 否决后仍是活窗口：外部关闭状态没有被误置，新的手势能重新发起一轮裁决。
        assert!(bridge.request());
        assert_eq!(bridge.decide(|| true), CloseAction::Close);
    }

    /// `windowWillClose:`（外部关闭）只走关闭记账：不投递事件、不询问裁决。
    #[test]
    fn an_external_close_never_takes_the_gesture_path() {
        let bridge = CloseBridge::new();
        assert!(bridge.will_close());
        assert!(!bridge.will_close(), "重复通知不得重复记账");
        assert_eq!(
            bridge.decide(|| panic!("外部关闭不得询问 on_close")),
            CloseAction::RemoveNative
        );
        assert_eq!(
            bridge.decide(|| panic!("外部关闭只记账一次")),
            CloseAction::Keep
        );
    }

    /// 外部关闭压过尚未裁决的手势：窗口已经被 AppKit 关掉，不能再投递 `CloseRequested`。
    #[test]
    fn an_external_close_supersedes_a_pending_gesture() {
        let bridge = CloseBridge::new();
        assert!(bridge.request());
        assert!(bridge.will_close());
        assert!(!bridge.request(), "关闭中的窗口不得再接受新的手势");
        assert_eq!(
            bridge.decide(|| panic!("已被 AppKit 关掉的窗口不得再询问裁决")),
            CloseAction::RemoveNative
        );
    }
}
