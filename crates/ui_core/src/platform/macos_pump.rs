use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RoundInputs {
    pub tick: bool,
    pub changed: bool,
    pub pending_wake: bool,
    pub timer_due: bool,
    pub continuous: bool,
    pub deferred: bool,
    pub a11y_retry: bool,
    pub a11y_backlog: bool,
    pub quit: bool,
}

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

    #[test]
    fn an_idle_vsync_tick_does_not_need_an_app_round() {
        assert!(!app_round(RoundInputs::default()));
    }

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
