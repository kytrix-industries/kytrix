//! macOS 泵的**平台无关纯逻辑**（Windows 开发机可单测）。
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
//!
//! 本模块同时放**一次性平台定时器**（`wake::set_deadline`）的复用判定（E10c-R06）：同样是
//! platform 无关、但决定唤醒是否丢失的纯逻辑，必须能在开发机上单测。

use std::time::Instant;

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

/// 平台一次性定时器（`wake::set_deadline`）是否可以直接复用槽里已挂的表。
///
/// `armed` = 槽里记录的 `(目标时刻, 平台定时器是否已经触发过)`；`now` = 本轮同步时刻。
///
/// 只有三者同时成立才复用：目标不变、仍在未来、**尚未触发**。一次性 `CFRunLoopTimer`
/// （`interval == 0`）触发后已被 CF 自动失效，槽里的对象不再会唤醒 run loop；若仍按「目标时刻
/// 相同且在将来」保留，泵在 target 之前被提前唤醒（CF 浮点换算 / 系统提前唤醒）时就不会重挂
/// ⇒ 唤醒丢失，直到下一次无关唤醒（E10c-R06；`run_due` 未到期不派发是对的，但平台表必须重挂）。
pub(crate) fn keep_armed_deadline(
    deadline: Option<Instant>,
    armed: Option<(Instant, bool)>,
    now: Instant,
) -> bool {
    match (deadline, armed) {
        (None, None) => true,
        (Some(target), Some((armed_target, false))) => armed_target == target && armed_target > now,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{RoundInputs, app_round, keep_armed_deadline};

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

    /// **E10c-R06 回归**：平台定时器提前触发（target 仍在未来）后，槽里的表已被 CF 失效 ⇒
    /// 下一轮必须重建，不能因为「目标时刻没变」就保留（否则这次唤醒丢失，直到下一次无关唤醒）。
    ///
    /// 变异守卫：`keep_armed_deadline` 删掉 `fired` 条件（只看目标与 now）⇒ 本用例变红。
    #[test]
    fn a_fired_platform_timer_is_rearmed_even_when_its_target_is_still_in_the_future() {
        let now = Instant::now();
        let target = now + Duration::from_millis(100);
        assert!(
            keep_armed_deadline(Some(target), Some((target, false)), now),
            "未触发 + 目标不变 + 仍在未来 ⇒ 复用（每轮重建 CF 对象是纯浪费）"
        );
        assert!(
            !keep_armed_deadline(Some(target), Some((target, true)), now),
            "已触发（CF 已失效）+ target 仍在未来 ⇒ 必须重建，否则唤醒丢失（R06）"
        );
    }

    /// 复用判定的其余边界：目标变化 / 已过期 / 取消 / 首次挂表 / 已触发的残留槽。
    #[test]
    fn deadline_reuse_boundaries_are_explicit() {
        let now = Instant::now();
        let target = now + Duration::from_millis(100);
        let other = target + Duration::from_millis(1);
        assert!(
            !keep_armed_deadline(
                Some(target),
                Some((target, false)),
                target + Duration::from_millis(1)
            ),
            "目标已过期 ⇒ 重建（fireDate 在过去时 CF 会立即触发）"
        );
        assert!(
            !keep_armed_deadline(Some(other), Some((target, false)), now),
            "目标变化（更晚）⇒ 重建"
        );
        assert!(
            !keep_armed_deadline(None, Some((target, false)), now),
            "取消 ⇒ 移除已挂表"
        );
        assert!(
            keep_armed_deadline(None, None, now),
            "既没有待挂也没有已挂 ⇒ 什么都不做"
        );
        assert!(
            !keep_armed_deadline(Some(target), None, now),
            "首次挂表 ⇒ 建"
        );
        assert!(
            !keep_armed_deadline(None, Some((target, true)), now),
            "已触发的残留槽 + 没有待挂 ⇒ 清理"
        );
    }

    /// **接线守护（E10c-R04 / 审查变异 M6）**：判据谓词是纯函数（上面三条用例），但把**真实
    /// 泵状态**接进 `RoundInputs` 的代码在 macOS-only 的 `platform/macos/event_loop.rs` 里：
    /// Windows 开发机不编译它，mac `timer_smoke` 也不注入输入 ⇒ 把 `changed` / `timer_due`
    /// 写成恒 `false` 可以存活全部自动化（E10c 审查实测 M6 PASS）。
    ///
    /// 本用例把 mac 泵的 gate 构造源码编译进测试（`include_str!` 只读源码，不要求目标平台），
    /// 断言 9 个判据字段都在、且没有任何字面量布尔。把任一字段改成 `false` / `true` ⇒ 本用例
    /// 变红 —— 这是「接线恒 false 必须红」的自动化守护。
    ///
    /// 边界：这是**源码级**接线守护（不是语义级）；真实 mac 输入路径的一次性验收证据仍以
    /// `timer_smoke` + 真机验收为准，delta 已登记。
    #[test]
    fn the_macos_gate_is_wired_to_real_pump_state() {
        const EVENT_LOOP: &str = include_str!("macos/event_loop.rs");
        let start = EVENT_LOOP
            .find("macos_pump::RoundInputs {")
            .expect("macOS 泵每轮必须构造 platform::macos_pump::RoundInputs");
        let rest = &EVENT_LOOP[start..];
        let gate = &rest[..rest.find("};").expect("RoundInputs 字面量必须以 `};` 收尾")];

        for field in [
            "tick",
            "changed",
            "pending_wake",
            "timer_due",
            "continuous",
            "deferred",
            "a11y_retry",
            "a11y_backlog",
            "quit",
        ] {
            assert!(
                gate.contains(field),
                "判据接线缺少字段 `{field}`（对应工作会停摆）"
            );
        }
        for literal in ["false", "true"] {
            assert!(
                !gate.contains(literal),
                "判据接线不得写死字面量 `{literal}`（M6 的恒 false 会漏掉真实工作且没有其它自动化守护）"
            );
        }
    }
}
