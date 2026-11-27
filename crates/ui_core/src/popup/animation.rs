use smallvec::SmallVec;

use crate::{
    app::App,
    interaction::Easing,
    math::LogicalPoint,
    node::tree::PopupTransform,
    popup::{
        POPUP_CLOSE_DURATION, POPUP_OPEN_DURATION, POPUP_SLIDE_DISTANCE, PopupAnimation,
        PopupAnimationKind, PopupAnimationState, PopupId, PopupState, PopupTier, Side,
    },
    window::WindowId,
};

fn slide_origin(side: Side) -> LogicalPoint {
    let distance = POPUP_SLIDE_DISTANCE;
    match side {
        Side::Bottom => LogicalPoint::new(crate::math::px(0.0), distance),
        Side::Top => LogicalPoint::new(crate::math::px(0.0), -distance),
        Side::Right => LogicalPoint::new(distance, crate::math::px(0.0)),
        Side::Left => LogicalPoint::new(-distance, crate::math::px(0.0)),
    }
}

pub(crate) fn start_open(app: &mut App, window: WindowId, id: PopupId) {
    let now = app.now();
    let Some(target) = app.window_mut(window) else {
        return;
    };
    let Ok(entry) = target.popups().entry(id) else {
        return;
    };
    let node = entry.node;
    let animation = entry.animation;
    let modal = entry.is_modal();
    let tier = entry.tier;
    let side = entry.placement.side;
    let offset = match animation {
        PopupAnimation::None | PopupAnimation::Fade => LogicalPoint::ZERO,
        PopupAnimation::FadeSlide => {
            if modal || tier == PopupTier::Modal {
                LogicalPoint::ZERO
            } else {
                slide_origin(side)
            }
        }
    };
    if animation == PopupAnimation::None {
        let _ = target.popups_mut().set_state(id, PopupState::Open);
        target.tree_mut().set_popup_transform(node, None);
        return;
    }
    target.tree_mut().set_popup_transform(
        node,
        Some(PopupTransform {
            offset,
            opacity: 0.0,
            blocked: false,
        }),
    );
    let _ = target.popups_mut().set_state(id, PopupState::Opening);
    target.popups_mut().push_animation(PopupAnimationState {
        id,
        kind: PopupAnimationKind::Opening,
        started: now,
        paint_offset: offset,
        opacity: 1.0,
    });
}

fn progress(state: &PopupAnimationState, now: std::time::Instant) -> f32 {
    let duration = match state.kind {
        PopupAnimationKind::Opening => POPUP_OPEN_DURATION,
        PopupAnimationKind::Closing => POPUP_CLOSE_DURATION,
    };
    let elapsed = now.saturating_duration_since(state.started);
    if duration.is_zero() {
        return 1.0;
    }
    (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
}

pub(crate) fn tick(app: &mut App, window: WindowId) {
    let Some(target) = app.window(window) else {
        return;
    };
    if !target.popups().animating() {
        return;
    }
    let now = app.now();
    let records: SmallVec<[PopupAnimationState; 8]> =
        target.popups().animations().iter().copied().collect();
    let mut finished_closing: SmallVec<[PopupId; 8]> = SmallVec::new();
    let mut finished_opening: SmallVec<[PopupId; 8]> = SmallVec::new();
    let mut damage: SmallVec<[crate::math::LogicalRect; 8]> = SmallVec::new();
    for record in records {
        let Some(target) = app.window_mut(window) else {
            return;
        };
        let Some((node, rect, state, previous)) =
            target.popups().entry(record.id).ok().map(|entry| {
                let rect = if entry.paint_rect.is_empty() {
                    entry.hit_rect
                } else {
                    entry.paint_rect
                };
                let node = entry.node;
                (node, rect, entry.state, target.tree().popup_transform(node))
            })
        else {
            finished_closing.push(record.id);
            continue;
        };
        let t = progress(&record, now);
        let eased = Easing::EaseOutCubic.sample(t);
        let (offset, opacity, done) = match record.kind {
            PopupAnimationKind::Opening => {
                let offset = LogicalPoint::new(
                    crate::math::px(record.paint_offset.x.value() * (1.0 - eased)),
                    crate::math::px(record.paint_offset.y.value() * (1.0 - eased)),
                );
                (offset, eased, t >= 1.0)
            }
            PopupAnimationKind::Closing => (
                record.paint_offset,
                record.opacity * (1.0 - eased),
                t >= 1.0,
            ),
        };
        if !rect.is_empty() {
            if let Some(previous) = previous {
                damage.push(rect.translate(previous.offset));
            }
            damage.push(rect.translate(offset));
        }
        if done && record.kind == PopupAnimationKind::Closing {
            finished_closing.push(record.id);
            continue;
        }
        let next = if done {
            None
        } else {
            Some(PopupTransform {
                offset,
                opacity,
                blocked: matches!(state, PopupState::DismissPending | PopupState::Closing),
            })
        };
        if previous != next {
            target.tree_mut().set_popup_transform(node, next);
            target.tree_mut().mark_paint(node);
        }
        if let Ok(entry) = target.popups_mut().entry_mut(record.id) {
            entry.paint_offset = if done { LogicalPoint::ZERO } else { offset };
        }
        if done {
            let _ = target.popups_mut().set_state(record.id, PopupState::Open);
            finished_opening.push(record.id);
        }
    }
    for rect in damage {
        if let Some(target) = app.window_mut(window) {
            target.request_damage(rect);
        }
    }
    for id in finished_opening {
        let node = app
            .window(window)
            .and_then(|target| target.popups().entry(id).ok().map(|entry| entry.node));
        if let Some(target) = app.window_mut(window) {
            if let Some(node) = node {
                target.tree_mut().set_popup_transform(node, None);
                target.tree_mut().mark_paint(node);
            }
            target.popups_mut().remove_animation(id);
        }
    }
    for id in finished_closing {
        super::dismiss::finalize_close(app, window, id);
    }
    if app
        .window(window)
        .is_some_and(|target| target.popups().animating())
    {
        app.request_frame();
    }
}
