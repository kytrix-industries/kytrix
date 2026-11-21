//! macOS 泵的**空闲轮判定**（平台无关纯逻辑；Windows 开发机可单测）。
//!
//! # 背景（E10c）
//!
//! macOS 泵由 `CFRunLoopObserver`（`BeforeWaiting`，**每轮 run loop 一次**）驱动，而
//! `CAMetalDisplayLink` 挂在主 run loop 上、按刷新率唤醒 run loop。R06-8 的空闲滞回
//! （`IDLE_GRACE = 250 ms`，`render::present_evidence`）让 display link 在最后一次工作之后
//! 继续跑一段时间 ⇒ 一次 `App::after` 的等待期里，泵会被 display link 的 vsync tick 空转
//! 几十轮（macOS 真机实测 54 轮 / 400 ms；Windows 同代码 1 轮）。
//!
//! # 判据
//!
//! **完整应用轮**（`App::run_frame_hook` / 帧管线 / 光标 / 任务 / IME / 连续帧定时器对账）
//! 只在**真有工作**时跑。纯 vsync tick 的空闲轮只做「事件与窗口状态预扫描 + 渲染器反馈 +
//! display link pacing + a11y 投递对账」，不调用任何应用回调 —— 等待期间没有应用级周期唤醒
//! （I3）。事件 / 窗口状态 / 跨线程唤醒 / 到点定时器 / 动画 / a11y / 退出 / 任务都在判据里，
//! 因此不会把真实工作误判成空闲。
//!
//! 判据放在本模块（而不是 `platform::macos`）的原因与 [`super::macos_owner`] /
//! [`super::macos_close`] 相同：`platform::macos` 整模块是 macOS-only（objc2 依赖），而这条
//! 不变量必须在开发机（Windows）上被真实单测覆盖。

/// 一轮泵的「需要完整应用轮」判据输入。
///
/// 全部来自泵已经算好的 O(1) 既有状态：无分配、无系统调用、不改任何状态。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RoundInputs {
    /// 本轮由连续帧 / 一次性定时器回调触发（`tick`）：定时器到点必须派发。
    pub tick: bool,
    /// 预扫描看到了变化：输入事件已派发 / 窗口状态或可见性变了 / 渲染器恢复过。
    pub changed: bool,
    /// `Dispatcher` 的帧唤醒位（跨线程唤醒 / `AppHandle::wake` / 任务回投 / a11y 动作）。
    pub pending_wake: bool,
    /// 最早的一次性定时器**已经到期**（平台唤醒偶尔早到时的兜底；未到期不算工作）。
    pub timer_due: bool,
    /// 有动画在跑（连续帧源）。
    pub continuous: bool,
    /// 有被帧预算门禁推迟的帧（下一个 vsync 必须补出）。
    pub deferred: bool,
    /// a11y 补发定时器到期（与连续帧定时器共用；到期必须交付）。
    pub a11y_retry: bool,
    /// 还有未走完的 a11y 批次（分批首树 / DPI 全量重建）。
    pub a11y_backlog: bool,
    /// 退出握手在等泵（`applicationShouldTerminate:` 只置位 + 唤醒，泵里裁决）。
    pub quit: bool,
}

/// 本轮是否需要跑完整应用轮。
///
/// 全部为假 ⇒ 本轮只是 display link 的 vsync tick（或无关的 run loop 唤醒）：只做空闲轮的
/// 渲染器 pacing 与 a11y 对账，不调用应用回调、不出帧。
///
/// 变异守卫：删掉任一字段（或让本函数恒返回 `true`）会让 `timer_smoke` 的「等待期轮数」
/// 从 1 跳到几十（macOS），或让对应工作停摆。
pub(crate) fn app_round(inputs: RoundInputs) -> bool {
    let RoundInputs {
        tick,
        changed,
        pending_wake,
        timer_due,
        continuous,
        deferred,
        a11y_retry,
        a11y_backlog,
        quit,
    } = inputs;
    tick || changed
        || pending_wake
        || timer_due
        || continuous
        || deferred
        || a11y_retry
        || a11y_backlog
        || quit
}

#[cfg(test)]
mod tests {
    use super::{RoundInputs, app_round};

    /// 空闲 vsync tick：全部判据为假 ⇒ **不**跑应用轮（I3 等待期只有触发轮）。
    #[test]
    fn an_idle_vsync_tick_does_not_need_an_app_round() {
        assert!(!app_round(RoundInputs::default()));
    }

    /// 每个判据单独为真都必须跑应用轮：少一条都会让对应工作停摆（动画、定时器、输入…）。
    #[test]
    fn every_round_input_alone_requires_an_app_round() {
        let cases: [(&str, RoundInputs); 9] = [
            (
                "tick",
                RoundInputs {
                    tick: true,
                    ..RoundInputs::default()
                },
            ),
            (
                "changed",
                RoundInputs {
                    changed: true,
                    ..RoundInputs::default()
                },
            ),
            (
                "pending_wake",
                RoundInputs {
                    pending_wake: true,
                    ..RoundInputs::default()
                },
            ),
            (
                "timer_due",
                RoundInputs {
                    timer_due: true,
                    ..RoundInputs::default()
                },
            ),
            (
                "continuous",
                RoundInputs {
                    continuous: true,
                    ..RoundInputs::default()
                },
            ),
            (
                "deferred",
                RoundInputs {
                    deferred: true,
                    ..RoundInputs::default()
                },
            ),
            (
                "a11y_retry",
                RoundInputs {
                    a11y_retry: true,
                    ..RoundInputs::default()
                },
            ),
            (
                "a11y_backlog",
                RoundInputs {
                    a11y_backlog: true,
                    ..RoundInputs::default()
                },
            ),
            (
                "quit",
                RoundInputs {
                    quit: true,
                    ..RoundInputs::default()
                },
            ),
        ];
        for (name, inputs) in cases {
            assert!(app_round(inputs), "判据 `{name}` 单独为真时必须跑应用轮");
        }
    }

    /// 组合判据：任一为真即应用轮（不是 AND，也不会被其它为假的值覆盖）。
    #[test]
    fn any_combination_of_inputs_keeps_the_app_round_alive() {
        let inputs = RoundInputs {
            timer_due: true,
            a11y_retry: true,
            ..RoundInputs::default()
        };
        assert!(app_round(inputs));
        let inputs = RoundInputs {
            tick: true,
            quit: true,
            ..RoundInputs::default()
        };
        assert!(app_round(inputs));
    }
}
