#[cfg(any(test, feature = "test-support"))]
use std::time::Duration;
use std::time::Instant;

use super::{AnimationCompletion, AnimationEndKind, AnimationSlot, AnimationTarget, App};
use crate::{
    error::EngineError,
    interaction::{AnimatedValue, AnimationId, AnimationSpec, animation as anim},
    math::LogicalPoint,
    node::{id::NodeId, signal::SignalId},
    paint::color::Color,
    window::WindowId,
};

pub(crate) fn scroll_animation_target(
    app: &App,
    node: NodeId,
    window: WindowId,
) -> Option<LogicalPoint> {
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

pub(crate) fn cancel_scroll_animation(app: &mut App, handle: &crate::interaction::ScrollHandle) {
    app.animations.retain(|slot| {
        !matches!(
            &slot.target,
            AnimationTarget::Scroll { handle: target, .. } if *target == *handle
        )
    });
}

fn queue_completion(app: &mut App, id: AnimationId, kind: AnimationEndKind) {
    app.animation_completions
        .push_back(AnimationCompletion { id, kind });
    app.dispatcher.wake();
}

fn take_same_target_slots(app: &mut App, target: &AnimationTarget) -> Vec<AnimationId> {
    let mut replaced = Vec::new();
    app.animations.retain(|slot| {
        let hit = slot.target.same_target(target);
        if hit && !matches!(slot.target, AnimationTarget::Scroll { .. }) {
            replaced.push(AnimationId::new(slot.window, slot.id));
        }
        !hit
    });
    replaced
}

pub(crate) fn dispatch_completions(app: &mut App) {
    if app.dispatching_animation_completions || app.animation_completions.is_empty() {
        return;
    }
    app.dispatching_animation_completions = true;
    if let Some(mut handler) = app.animation_complete_handler.take() {
        while let Some(event) = app.animation_completions.pop_front() {
            handler(app, event);
        }
        if app.animation_complete_handler.is_none() {
            app.animation_complete_handler = Some(handler);
        }
    } else {
        app.animation_completions.clear();
    }
    app.dispatching_animation_completions = false;
}

pub(crate) fn release_scope_animations(app: &mut App, signals: &[SignalId]) {
    let mut released = Vec::new();
    app.animations.retain(|slot| {
        let hit = slot.target.targets_any(signals);
        if hit && !matches!(slot.target, AnimationTarget::Scroll { .. }) {
            released.push(AnimationId::new(slot.window, slot.id));
        }
        !hit
    });
    for id in released {
        queue_completion(app, id, AnimationEndKind::Released);
    }
}

pub(crate) fn drop_completions_for_window(app: &mut App, window: WindowId) {
    app.animation_completions
        .retain(|event| event.id.window() != window);
}

pub(crate) fn cancel_node_animations(
    app: &mut App,
    window: WindowId,
    removed: &ahash::HashSet<NodeId>,
) -> usize {
    let mut count = 0usize;
    let mut cancelled: Vec<AnimationId> = Vec::new();
    app.animations.retain_mut(|slot| {
        if slot.window != window {
            return true;
        }
        if let AnimationTarget::Scroll { handle, .. } = &slot.target
            && removed.contains(&handle.node())
        {
            count += 1;
            return false;
        }
        let had_nodes = !slot.nodes.is_empty();
        slot.nodes.retain(|node| !removed.contains(node));
        if had_nodes && slot.nodes.is_empty() {
            cancelled.push(AnimationId::new(slot.window, slot.id));
            count += 1;
            return false;
        }
        true
    });
    for id in cancelled {
        queue_completion(app, id, AnimationEndKind::Removed);
    }
    count
}

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
                settle_immediately(
                    app,
                    &slot_target,
                    window,
                    nodes,
                    AnimationId::new(window, id),
                );
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
                settle_immediately(
                    app,
                    &slot_target,
                    window,
                    nodes,
                    AnimationId::new(window, id),
                );
                return Ok(AnimationId::new(window, id));
            }
            slot_target
        }
    };
    let replaced = take_same_target_slots(app, &target);
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
    for id in replaced {
        queue_completion(app, id, AnimationEndKind::Replaced);
    }
    Ok(AnimationId::new(window, id))
}

fn settle_immediately(
    app: &mut App,
    target: &AnimationTarget,
    window: WindowId,
    nodes: &[NodeId],
    id: AnimationId,
) {
    let replaced = take_same_target_slots(app, target);
    match target {
        AnimationTarget::Scalar { transient, to, .. } => {
            app.store.set_transient(transient.id(), *to)
        }
        AnimationTarget::Color { transient, to, .. } => {
            app.store.set_transient(transient.id(), *to)
        }
        AnimationTarget::Scroll { .. } => {
            debug_assert!(false, "zero-duration scroll animation is not reachable");
        }
    }
    app.mark_nodes_paint(window, nodes);
    app.request_frame();
    for id in replaced {
        queue_completion(app, id, AnimationEndKind::Replaced);
    }
    queue_completion(app, id, AnimationEndKind::Finished);
}

pub(crate) fn cancel_animation(app: &mut App, id: &AnimationId) -> bool {
    let before = app.animations.len();
    let mut cancelled = Vec::new();
    app.animations.retain(|slot| {
        let hit = slot.window == id.window() && slot.id == id.generation();
        if hit && !matches!(slot.target, AnimationTarget::Scroll { .. }) {
            cancelled.push(AnimationId::new(slot.window, slot.id));
        }
        !hit
    });
    for id in cancelled {
        queue_completion(app, id, AnimationEndKind::Cancelled);
    }
    app.animations.len() != before
}

#[must_use]
pub(crate) fn animations_running(app: &App, window: WindowId) -> bool {
    app.animations.iter().any(|slot| slot.window == window)
}

pub(crate) fn has_animations(app: &App) -> bool {
    !app.animations.is_empty()
}

pub(crate) fn needs_continuous_frames(app: &App) -> bool {
    app.has_animations()
}

pub(crate) fn mark_nodes_paint(app: &mut App, window: WindowId, nodes: &[NodeId]) {
    if let Some(target) = app.windows.get_mut(&window) {
        for node in nodes {
            target.tree_mut().mark_bind(*node);
            target.tree_mut().mark_paint(*node);
        }
    }
}

pub(crate) fn tick_animations(app: &mut App, window: WindowId) {
    if !app.animations_running(window) {
        return;
    }
    let now = app.clock;
    let slots = std::mem::take(&mut app.animations);
    let mut keep = Vec::with_capacity(slots.len());
    let mut finished: Vec<AnimationId> = Vec::new();
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
                target.tree_mut().mark_bind(*node);
                target.tree_mut().mark_paint(*node);
            }
        }
        if t < 1.0 {
            keep.push(slot);
        } else if !matches!(slot.target, AnimationTarget::Scroll { .. }) {
            finished.push(AnimationId::new(slot.window, slot.id));
        }
    }
    app.animations = keep;
    for id in finished {
        queue_completion(app, id, AnimationEndKind::Finished);
    }
    dispatch_completions(app);
}

pub(crate) fn sync_clock(app: &mut App) {
    app.clock = Instant::now();
}

pub(crate) fn sync_clock_live(app: &mut App) {
    app.clock = Instant::now();
    app.clock_live = true;
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn advance_clock(app: &mut App, by: Duration) {
    app.clock += by;
}

#[cfg(all(test, feature = "hud"))]
mod tests {
    use std::time::Duration;

    use super::{has_animations, needs_continuous_frames};
    use crate::{
        app::App,
        interaction::{AnimatedValue, AnimationSpec, Easing},
        math::{LogicalRect, LogicalSize, px},
        node::builder::NodeBuilder,
        window::WindowOptions,
    };

    #[test]
    fn the_debug_hud_never_asks_for_continuous_frames() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            size: LogicalSize::new(px(320.0), px(240.0)),
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(32.0), px(32.0)))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("node");

        assert!(!needs_continuous_frames(&app), "没有动画 ⇒ 不需要连续帧");

        app.set_debug_hud(window, true).expect("HUD 安装");
        assert!(
            !needs_continuous_frames(&app),
            "装 HUD 不是持续出帧的理由（纯观察者；变异「HUD 可见 ⇒ 连续帧」会在这里变红）"
        );

        let value = app.transient(0.0_f32);
        app.animate(
            window,
            AnimatedValue::Scalar {
                target: value,
                to: 1.0,
            },
            &[node],
            AnimationSpec {
                duration: Duration::from_millis(50),
                easing: Easing::Linear,
            },
        )
        .expect("animate");
        assert!(needs_continuous_frames(&app), "有动画 ⇒ 需要连续帧");

        app.advance_clock(Duration::from_millis(60));
        let _ = app.frame(window);
        assert!(!has_animations(&app), "前置条件：动画已经结束");
        assert!(
            !needs_continuous_frames(&app),
            "动画结束 + HUD 可见 ⇒ 仍然不需要连续帧"
        );
    }
}
