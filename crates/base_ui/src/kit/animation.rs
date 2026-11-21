//! 弹层淡入淡出的动画语义（旧 `crate::animation` 的引擎化移植）。
//!
//! # 与旧实现的关系
//!
//! 旧模块只有两样东西：`ANIMATION_TICK`（帧步长）与 `popup_should_render(opened, PresenceSample)`
//! （弹层这一帧要不要进树）。新实现保留**语义**，把 GDUI `PresenceSample` 换成进度的 `f32`：
//!
//! - `tick` 来自主题度量 token（[`ComponentTokens::animation_tick`]，旧的 150 ms）；
//! - 「这一帧要不要画」的判据是 `opened || progress > 0.0`：弹层打开时恒画；关闭后只在淡出
//!   还没走完（`progress > 0`）时继续画。旧实现不能直接用 `PresenceSample::should_render`
//!   （它只看 phase），原因见旧模块注释：元素第一次被采样时 phase 是 `Exiting`、进度恒 0，
//!   画出来就是一个不可见但**挡住指针**的弹层。
//!
//! # 引擎化：不建帧循环
//!
//! 淡入淡出**不**在本模块推进：透明度是引擎 `Transient<f32>`，由 `App::animate(window,
//! AnimatedValue::Scalar { target, to }, &[nodes], AnimationSpec { .. })` 逐帧推进（设计 §3.7：
//! 高频状态与动画不进依赖图）。组件只做两件事：
//!
//! 1. 挂载时用 `bind_style` 读 `Transient` 定不透明度；
//! 2. 打开 / 关闭时注册一次动画，并把本节点加入 `nodes`（引擎只重绘显式注册的节点集合）。
//!
//! 本模块因此只有纯函数；`tick` 仅用于「需要自己按固定步长做非插值动画」的少数场景
//! （旧实现的 150 ms 帧步长），**不是**通用帧源——引擎的 `needs_continuous_frames` 只由
//! `App::animate` 注册的动画驱动。

use std::time::Duration;

use crate::theme::components::ComponentTokens;

/// 旧 `ANIMATION_TICK` 的引擎化读取（唯一数值来源：主题度量 token）。
#[must_use]
pub fn animation_tick() -> Duration {
    ComponentTokens::standard().animation_tick
}

/// 弹层这一帧是否要进树 / 参与绘制。
///
/// `opened` = 应用语义上的「现在是打开的」；`progress` = 淡出的进度（`0` = 完全透明，
/// `1` = 完全不透明）。判据：`opened || progress > 0`。
///
/// 注意：新引擎里「不可见但挡住指针」的问题由**节点级 `interactive`** 解决（`StylePatch` 的
/// `visible` / `interactive` 字段），本函数只回答「要不要画」，不替代交互开关。
#[must_use]
pub fn popup_should_render(opened: bool, progress: f32) -> bool {
    opened || progress > 0.0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use gpui_kit::base::{MotionStatus, PresencePhase, PresenceSample};
    use ui_core::interaction::{AnimatedValue, AnimationSpec, Easing};

    use super::{animation_tick, popup_should_render};
    use crate::kit::test_support::test_app;

    /// `tick` 与旧常量 / 主题 token 一致（旧常量是 150 ms）。
    #[test]
    fn the_animation_tick_is_the_themed_legacy_interval() {
        assert_eq!(animation_tick(), Duration::from_millis(150));
        assert_eq!(animation_tick(), crate::animation::ANIMATION_TICK);
    }

    /// 判据与旧 `popup_should_render` 逐值等价（旧 `PresenceSample` 的字段是公开的，直接造样本）。
    #[test]
    fn the_render_predicate_matches_the_legacy_implementation() {
        for opened in [false, true] {
            for phase in [
                PresencePhase::Entering,
                PresencePhase::Present,
                PresencePhase::Exiting,
                PresencePhase::Absent,
            ] {
                for progress in [0.0f32, 0.25, 1.0] {
                    let sample = PresenceSample {
                        phase,
                        progress,
                        status: MotionStatus::Running,
                    };
                    assert_eq!(
                        popup_should_render(opened, progress),
                        crate::animation::popup_should_render(opened, sample),
                        "opened={opened} phase={phase:?} progress={progress} 的判据漂移"
                    );
                }
            }
        }
    }

    /// 真值表：关闭且淡出已结束 ⇒ 不进树；打开或淡出中 ⇒ 进树。
    #[test]
    fn the_truth_table_matches_the_legacy_intent() {
        assert!(!popup_should_render(false, 0.0));
        assert!(popup_should_render(false, 0.001));
        assert!(popup_should_render(false, 1.0));
        assert!(popup_should_render(true, 0.0));
        assert!(popup_should_render(true, 1.0));
        // NaN 进度（引擎瞬态值异常）不 panic、按「不画」处理。
        assert!(!popup_should_render(false, f32::NAN));
    }

    /// 淡入淡出由引擎的动画时钟驱动（`Transient` + `App::animate`），本模块不参与帧推进。
    #[test]
    fn the_engine_animation_clock_drives_the_opacity_transient() {
        let mut app = test_app();
        let window = app.window_id();
        let opacity = app.app_mut().transient(0.0f32);
        let spec = AnimationSpec {
            duration: Duration::from_millis(100),
            easing: Easing::Linear,
        };
        app.app_mut()
            .animate(
                window,
                AnimatedValue::Scalar {
                    target: opacity,
                    to: 1.0,
                },
                &[],
                spec,
            )
            .expect("注册动画");
        assert!(app.app().animations_running(window), "动画已注册");

        let _ = app.drain().expect("首帧");
        app.advance(Duration::from_millis(150));
        let _ = app.drain().expect("动画帧");
        let value = *app.app().read_transient(&opacity);
        assert_eq!(value, 1.0, "动画结束后瞬态值必须到目标值");
        assert!(!app.app().animations_running(window), "动画结束即停帧");
    }
}
