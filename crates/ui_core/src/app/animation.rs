//! 动画槽与引擎时钟：注册 / 推进 / 取消，以及「是否需要持续出帧」。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

#[cfg(any(test, feature = "test-support"))]
use std::time::Duration;
use std::time::Instant;

use super::{AnimationSlot, AnimationTarget, App};
use crate::{
    error::EngineError,
    interaction::{AnimatedValue, AnimationId, AnimationSpec, animation as anim},
    math::Point,
    node::id::NodeId,
    paint::color::Color,
    window::WindowId,
};

/// 该节点正在进行中的滚动动画目标（没有动画时 `None`）。
/// 消费者：`App::scroll_animation_target`。
pub(crate) fn scroll_animation_target(app: &App, node: NodeId, window: WindowId) -> Option<Point> {
    app.animations.iter().rev().find_map(|slot| {
        if slot.window != window {
            return None;
        }
        match &slot.target {
            AnimationTarget::Scroll { handle, to, .. } if handle.node() == node => Some(*to),
            _ => None,
        }
    })
}

/// 取消某个滚动槽上的动画（精确滚动接管时用；R02-10）。
///
/// 触控板的像素级滚动必须 1:1 跟随手指：若上一次离散滚轮登记的动画还在跑，它会在同一帧
/// 继续写 `offset`，与手指位移互相打断（表现为「黏手」）。所以精确滚动一开始就接管目标。
/// 消费者：`app::input::on_scroll`。
pub(crate) fn cancel_scroll_animation(app: &mut App, handle: &crate::interaction::ScrollHandle) {
    app.animations.retain(|slot| {
        !matches!(
            &slot.target,
            AnimationTarget::Scroll { handle: target, .. } if *target == *handle
        )
    });
}

// ── P10：动画 ──────────────────────────────────────────────────────────────

/// 注册一个动画（起点 = 目标瞬态值的当前值；同目标已有动画 ⇒ 替换）。
///
/// `nodes` 是**显式注册**的「每帧要重绘的节点集合」（对应设计 §3.7）。
///
/// # Errors
/// `UnknownWindow`（窗口已关闭）。
/// 消费者：`App::animate`。
pub(crate) fn animate(
    app: &mut App,
    window: WindowId,
    value: AnimatedValue,
    nodes: &[NodeId],
    spec: AnimationSpec,
) -> Result<AnimationId, EngineError> {
    if !app.windows.contains_key(&window) {
        return Err(EngineError::UnknownWindow { window });
    }
    let id = app.next_animation;
    app.next_animation = app.next_animation.wrapping_add(1);
    let target = match value {
        AnimatedValue::Scalar { target, to } => {
            let from = app
                .store
                .transient_ref::<f32>(target.id())
                .copied()
                .unwrap_or(0.0);
            let slot_target = AnimationTarget::Scalar {
                transient: target,
                from,
                to,
            };
            if spec.duration.is_zero() {
                settle_immediately(app, &slot_target, window, nodes);
                return Ok(AnimationId::new(window, id));
            }
            slot_target
        }
        AnimatedValue::Color { target, to } => {
            let from = app
                .store
                .transient_ref::<Color>(target.id())
                .copied()
                .unwrap_or(Color::TRANSPARENT);
            let slot_target = AnimationTarget::Color {
                transient: target,
                from,
                to,
            };
            if spec.duration.is_zero() {
                settle_immediately(app, &slot_target, window, nodes);
                return Ok(AnimationId::new(window, id));
            }
            slot_target
        }
    };
    // 同一目标重定向：旧槽必须让位（否则两条插值同时写同一个瞬态值 ⇒ 回弹/抖动）。
    app.animations
        .retain(|slot| !slot.target.same_target(&target));
    app.animations.push(AnimationSlot {
        id,
        window,
        target,
        start: app.clock,
        duration: spec.duration,
        easing: spec.easing,
        nodes: nodes.to_vec(),
    });
    app.request_frame();
    Ok(AnimationId::new(window, id))
}

/// 零时长动画：**立即到位** + 取消同一目标上的旧槽（R01-13）。
///
/// 旧实现只在 `duration == 0` 分支里写一次瞬态值就 `return`，**不清旧槽**：上一次 `animate`
/// 注册的长动画仍在 `app.animations` 里，同一帧的 `tick_animations` 会继续把同一个
/// `Transient` 推向旧目标值（「设成 0 ms 还是被拉回 1.0」的回弹）。零时长的语义是
/// 「不要动画」，所以与非零路径同一规则：同一目标只能有一条驱动源。
fn settle_immediately(app: &mut App, target: &AnimationTarget, window: WindowId, nodes: &[NodeId]) {
    app.animations
        .retain(|slot| !slot.target.same_target(target));
    match target {
        AnimationTarget::Scalar { transient, to, .. } => {
            app.store.set_transient(transient.id(), *to)
        }
        AnimationTarget::Color { transient, to, .. } => {
            app.store.set_transient(transient.id(), *to)
        }
        // `AnimatedValue` 没有滚动变体：滚动动画只由 `App::scroll_to` 内部注册（不经本函数）。
        AnimationTarget::Scroll { .. } => {
            debug_assert!(false, "zero-duration scroll animation is not reachable");
        }
    }
    app.mark_nodes_paint(window, nodes);
    app.request_frame();
}

/// 取消动画（幂等）。
/// 消费者：`App::cancel_animation`。
pub(crate) fn cancel_animation(app: &mut App, id: &AnimationId) -> bool {
    let before = app.animations.len();
    app.animations
        .retain(|slot| !(slot.window == id.window() && slot.id == id.generation()));
    app.animations.len() != before
}

/// 该窗口是否还有未完成的动画。
/// 消费者：`App::animations_running`。
#[must_use]
pub(crate) fn animations_running(app: &App, window: WindowId) -> bool {
    app.animations.iter().any(|slot| slot.window == window)
}

/// 全部窗口是否还有未完成的动画（平台泵用它决定 `SetTimer(1)` / `KillTimer`）。
/// 消费者：`App::has_animations`。
pub(crate) fn has_animations(app: &App) -> bool {
    !app.animations.is_empty()
}

/// 平台是否需要持续出帧：动画，或任一窗口的 Debug HUD 可见。
/// 消费者：`App::needs_continuous_frames`。
pub(crate) fn needs_continuous_frames(app: &App) -> bool {
    #[cfg(feature = "hud")]
    {
        app.has_animations()
            || app.debug_hud.iter().any(|(window, state)| {
                app.windows
                    .get(window)
                    .is_some_and(|target| target.tree().get(state.root()).is_ok())
            })
    }
    #[cfg(not(feature = "hud"))]
    {
        app.has_animations()
    }
}

/// 消费者：`App::mark_nodes_paint`。
pub(crate) fn mark_nodes_paint(app: &mut App, window: WindowId, nodes: &[NodeId]) {
    if let Some(target) = app.windows.get_mut(&window) {
        for node in nodes {
            // 动画写的是 `Transient`，而 `bind_style` 读它 ⇒ 必须重算动态属性（`BIND`）并重绘
            // （`PAINT`）。这是 `BIND` / `PAINT` 拆开后的两个独立通道（R03-9）。
            target.tree_mut().mark_bind(*node);
            target.tree_mut().mark_paint(*node);
        }
    }
}

/// 推进本窗口的动画（`App::frame` 在帧门禁**之前**调用）。
/// 消费者：`App::tick_animations`。
///
/// # R01-7 的剩余项（登记，不在本次改动范围）
///
/// 每帧仍有一次 `mem::take` + `Vec::with_capacity`，以及每个被驱动节点一次
/// `windows.get_mut`。两处都能消（复用 `App` 上的 scratch 缓冲、把窗口查找提到循环外），
/// 但它们是**纯分配/借用顺序**优化：现有动画语义测试在两种实现下都绿，拿不出"先红后绿"的
/// 行为断言；按本轮的门禁口径（每条修复必须带能失败的测试）不能算完成，留待与帧基准一起做。
pub(crate) fn tick_animations(app: &mut App, window: WindowId) {
    if !app.animations_running(window) {
        return;
    }
    let now = app.clock;
    let slots = std::mem::take(&mut app.animations);
    let mut keep = Vec::with_capacity(slots.len());
    for slot in slots {
        if slot.window != window {
            keep.push(slot);
            continue;
        }
        let t = anim::progress(now.saturating_duration_since(slot.start), slot.duration);
        let eased = slot.easing.sample(t);
        match &slot.target {
            AnimationTarget::Scalar {
                transient,
                from,
                to,
            } => {
                app.store
                    .set_transient(transient.id(), anim::lerp(*from, *to, eased));
            }
            AnimationTarget::Color {
                transient,
                from,
                to,
            } => {
                app.store
                    .set_transient(transient.id(), anim::lerp_color(*from, *to, eased));
            }
            AnimationTarget::Scroll { handle, from, to } => {
                let point = anim::lerp_point(*from, *to, eased);
                let _ = app.scroll_to(handle, point);
            }
        }
        for node in &slot.nodes {
            if let Some(target) = app.windows.get_mut(&window) {
                // 同上：瞬态值变了 ⇒ 读到它的 `bind_style` 必须重算，然后才谈重绘。
                target.tree_mut().mark_bind(*node);
                target.tree_mut().mark_paint(*node);
            }
        }
        if t < 1.0 {
            keep.push(slot);
        }
    }
    app.animations = keep;
}

/// 把引擎时钟推进到真实时间（平台泵每轮一次；测试用 `TestApp::advance`）。
/// 消费者：`App::sync_clock`。
pub(crate) fn sync_clock(app: &mut App) {
    app.clock = Instant::now();
}

/// 推进测试时钟（`TestApp::advance` 的唯一实现；只在测试构建里存在）。
/// 消费者：`App::advance_clock`。
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn advance_clock(app: &mut App, by: Duration) {
    app.clock += by;
}
