use std::time::{Duration, Instant};

use smallvec::SmallVec;

use crate::{
    app::{App, refresh_hover_under_pointer},
    event::{ScrollPhase, ScrollUnit},
    interaction::{ScrollHandle, ScrollSlot},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{id::NodeId, tree::NodeTree},
    window::WindowId,
};

pub(crate) const MAX_CHAIN_HOPS: usize = 32;
pub(crate) const SCROLL_SEQUENCE_IDLE: Duration = Duration::from_millis(200);
pub(crate) const EPS: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionKind {
    Discrete,
    Continuous,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollSession {
    pub target: NodeId,
    pub kind: SessionKind,
    pub last_event_at: Instant,
    pub ended_at: Option<Instant>,
}

impl ScrollSession {
    pub(crate) fn is_expired(self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_event_at) > SCROLL_SEQUENCE_IDLE
    }
}

#[must_use]
pub(crate) fn clamp(value: LogicalPx, max: LogicalPx) -> LogicalPx {
    if max <= LogicalPx::ZERO {
        return LogicalPx::ZERO;
    }
    LogicalPx::new(value.value().clamp(0.0, max.value()))
}

#[must_use]
pub(crate) fn max_offset(slot: &ScrollSlot, viewport: LogicalSize) -> LogicalPoint {
    let max_x = if slot.axes.horizontal() {
        (slot.content.width - viewport.width).max(LogicalPx::ZERO)
    } else {
        LogicalPx::ZERO
    };
    let max_y = if slot.axes.vertical() {
        (slot.content.height - viewport.height).max(LogicalPx::ZERO)
    } else {
        LogicalPx::ZERO
    };
    LogicalPoint::new(max_x, max_y)
}

#[must_use]
pub(crate) fn clamp_offset(slot: &ScrollSlot, viewport: LogicalSize) -> LogicalPoint {
    let max = max_offset(slot, viewport);
    let x = if slot.axes.horizontal() {
        clamp(slot.offset.x, max.x)
    } else {
        LogicalPx::ZERO
    };
    let y = if slot.axes.vertical() {
        clamp(slot.offset.y, max.y)
    } else {
        LogicalPx::ZERO
    };
    LogicalPoint::new(x, y)
}

#[must_use]
pub(crate) fn viewport(tree: &NodeTree, node: NodeId) -> LogicalRect {
    tree.get(node).map_or_else(
        |_| LogicalRect::default(),
        |target| target.layout().bounds.inset(target.layout().border),
    )
}

pub(crate) fn scroll_ancestors(
    tree: &NodeTree,
    from: NodeId,
) -> impl Iterator<Item = (NodeId, ScrollSlot)> + '_ {
    let mut cursor = Some(from);
    std::iter::from_fn(move || {
        while let Some(node) = cursor {
            cursor = tree.get(node).ok().and_then(|target| target.parent());
            if let Some(slot) = tree.scroll_slot(node) {
                return Some((node, slot));
            }
        }
        None
    })
}

#[must_use]
pub(crate) fn scroll_ancestor(tree: &NodeTree, from: NodeId) -> Option<(NodeId, ScrollSlot)> {
    scroll_ancestors(tree, from).next()
}

#[must_use]
pub(crate) fn ancestor_offset(tree: &NodeTree, node: NodeId) -> LogicalPoint {
    let mut total = LogicalPoint::ZERO;
    let mut cursor = tree.get(node).ok().and_then(|target| target.parent());
    while let Some(current) = cursor {
        if let Some(slot) = tree.scroll_slot(current) {
            total = LogicalPoint::new(total.x + slot.offset.x, total.y + slot.offset.y);
        }
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    total
}

#[must_use]
pub(crate) fn to_screen(tree: &NodeTree, node: NodeId, rect: LogicalRect) -> LogicalRect {
    let offset = ancestor_offset(tree, node);
    rect.translate(LogicalPoint::new(-offset.x, -offset.y))
}

#[must_use]
pub(crate) fn scroll_candidates(tree: &NodeTree, from: NodeId) -> SmallVec<[NodeId; 8]> {
    let mut out = SmallVec::new();
    let mut visited = SmallVec::<[NodeId; 16]>::new();
    let mut cursor = Some(from);
    let mut hops = 0_usize;
    while let Some(node) = cursor {
        hops += 1;
        if hops > MAX_CHAIN_HOPS || visited.contains(&node) {
            break;
        }
        visited.push(node);
        if let Some(proxy) = tree.scroll_proxy(node)
            && let Some(target) = follow_proxy_chain(tree, proxy, &mut visited, &mut hops)
        {
            let _ = push_candidate(&mut out, target);
            cursor = tree.get(target).ok().and_then(|target| target.parent());
            continue;
        }
        if tree.scroll_slot(node).is_some() {
            let _ = push_candidate(&mut out, node);
        }
        cursor = tree.get(node).ok().and_then(|target| target.parent());
    }
    out
}

fn follow_proxy_chain(
    tree: &NodeTree,
    first: NodeId,
    visited: &mut SmallVec<[NodeId; 16]>,
    hops: &mut usize,
) -> Option<NodeId> {
    let mut current = first;
    loop {
        *hops += 1;
        if *hops > MAX_CHAIN_HOPS
            || visited.contains(&current)
            || tree.scroll_slot(current).is_none()
        {
            return None;
        }
        visited.push(current);
        match tree.scroll_proxy(current) {
            Some(next) => current = next,
            None => return Some(current),
        }
    }
}

fn push_candidate(candidates: &mut SmallVec<[NodeId; 8]>, node: NodeId) -> bool {
    if candidates.contains(&node) {
        return false;
    }
    candidates.push(node);
    true
}

fn set_scroll_session(app: &mut App, window: WindowId, session: Option<ScrollSession>) {
    if let Some(target) = app.window_mut(window) {
        target.pointer_mut().scroll_session = session;
    }
}

pub(crate) fn clear_scroll_session(app: &mut App, window: WindowId) {
    set_scroll_session(app, window, None);
}

#[must_use]
pub(crate) fn request_px(slot: &ScrollSlot, raw: LogicalPoint, unit: ScrollUnit) -> LogicalPoint {
    let lifted = match unit {
        ScrollUnit::Line => LogicalPoint::new(
            px(raw.x.value() * slot.line_height.value()),
            px(raw.y.value() * slot.line_height.value()),
        ),
        ScrollUnit::Pixel => raw,
    };
    LogicalPoint::new(px(-lifted.x.value()), px(-lifted.y.value()))
}

#[must_use]
pub(crate) fn applied_delta(
    slot: &ScrollSlot,
    viewport: LogicalSize,
    requested: LogicalPoint,
) -> LogicalPoint {
    let max = max_offset(slot, viewport);
    let x = if slot.axes.horizontal() {
        clamp(px(slot.offset.x.value() + requested.x.value()), max.x)
    } else {
        slot.offset.x
    };
    let y = if slot.axes.vertical() {
        clamp(px(slot.offset.y.value() + requested.y.value()), max.y)
    } else {
        slot.offset.y
    };
    LogicalPoint::new(x - slot.offset.x, y - slot.offset.y)
}

#[must_use]
pub(crate) fn smallest_angle_between(v1: LogicalPoint, v2: LogicalPoint) -> f32 {
    let (x1, y1) = (f64::from(v1.x.value()), f64::from(v1.y.value()));
    let (x2, y2) = (f64::from(v2.x.value()), f64::from(v2.y.value()));
    let len1 = (x1 * x1 + y1 * y1).sqrt();
    let len2 = (x2 * x2 + y2 * y2).sqrt();
    if len1 == 0.0 || len2 == 0.0 {
        return 0.0;
    }
    let dot = ((x1 * x2 + y1 * y2) / len1 / len2).clamp(-1.0, 1.0);
    dot.acos().to_degrees() as f32
}

#[must_use]
pub(crate) fn project_vector(source: LogicalPoint, destination: LogicalPoint) -> LogicalPoint {
    let (dx, dy) = (
        f64::from(destination.x.value()),
        f64::from(destination.y.value()),
    );
    let length_squared = dx * dx + dy * dy;
    if length_squared == 0.0 {
        return source;
    }
    let factor =
        (f64::from(source.x.value()) * dx + f64::from(source.y.value()) * dy) / length_squared;
    LogicalPoint::new(px((factor * dx) as f32), px((factor * dy) as f32))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollApplication {
    pub applied: LogicalPoint,
    pub consumed: LogicalPoint,
    pub moved: bool,
}

#[must_use]
pub(crate) fn apply_scroll(
    slot: &ScrollSlot,
    viewport: LogicalSize,
    requested: LogicalPoint,
) -> ScrollApplication {
    let applied = applied_delta(slot, viewport, requested);
    if applied.x.value().abs() <= EPS && applied.y.value().abs() <= EPS {
        return ScrollApplication {
            applied,
            consumed: LogicalPoint::ZERO,
            moved: false,
        };
    }
    let angle = smallest_angle_between(applied, requested);
    let consumed = if angle < 45.0 {
        requested
    } else {
        project_vector(requested, applied)
    };
    ScrollApplication {
        applied,
        consumed,
        moved: true,
    }
}

#[must_use]
pub(crate) fn select_target(
    tree: &NodeTree,
    candidates: &[NodeId],
    raw: LogicalPoint,
    unit: ScrollUnit,
) -> Option<NodeId> {
    candidates.iter().copied().find(|candidate| {
        if !tree.is_effectively_visible(*candidate) || !tree.is_effectively_interactive(*candidate)
        {
            return false;
        }
        let Some(slot) = tree.scroll_slot(*candidate) else {
            return false;
        };
        let viewport = viewport(tree, *candidate).size;
        let requested = request_px(&slot, raw, unit);
        let applied = applied_delta(&slot, viewport, requested);
        applied.x.value().abs() > EPS || applied.y.value().abs() > EPS
    })
}

fn session_target_alive(app: &App, window: WindowId, target: NodeId) -> bool {
    let Some(target_window) = app.window(window) else {
        return false;
    };
    let tree = target_window.tree();
    tree.scroll_slot(target).is_some()
        && tree.is_effectively_visible(target)
        && tree.is_effectively_interactive(target)
}

fn session_kind(phase: ScrollPhase) -> SessionKind {
    match phase {
        ScrollPhase::Discrete => SessionKind::Discrete,
        ScrollPhase::Begin
        | ScrollPhase::Update
        | ScrollPhase::End
        | ScrollPhase::Momentum
        | ScrollPhase::Cancel => SessionKind::Continuous,
    }
}

fn settle_phase_without_scroll(
    app: &mut App,
    window: WindowId,
    phase: ScrollPhase,
    session: Option<ScrollSession>,
    timestamp: Instant,
) {
    match phase {
        ScrollPhase::Cancel | ScrollPhase::Begin => set_scroll_session(app, window, None),
        ScrollPhase::End => {
            let ended = session.and_then(|active| {
                (active.kind == SessionKind::Continuous).then_some(ScrollSession {
                    last_event_at: timestamp,
                    ended_at: Some(timestamp),
                    ..active
                })
            });
            set_scroll_session(app, window, ended);
        }
        ScrollPhase::Discrete | ScrollPhase::Update | ScrollPhase::Momentum => {
            if let Some(active) = session {
                set_scroll_session(
                    app,
                    window,
                    Some(ScrollSession {
                        last_event_at: timestamp,
                        ..active
                    }),
                );
            }
        }
    }
}

pub(crate) fn route_wheel(
    app: &mut App,
    window: WindowId,
    hit: Option<NodeId>,
    raw: LogicalPoint,
    unit: ScrollUnit,
    phase: ScrollPhase,
    timestamp: Instant,
) {
    let Some(target_window) = app.window(window) else {
        return;
    };
    let mut session = target_window.pointer().scroll_session;
    if session.is_some_and(|active| active.is_expired(timestamp)) {
        session = None;
        clear_scroll_session(app, window);
    }
    if phase == ScrollPhase::End
        && session.is_none_or(|active| !session_target_alive(app, window, active.target))
    {
        clear_scroll_session(app, window);
        return;
    }
    if hit.is_none() {
        settle_phase_without_scroll(app, window, phase, session, timestamp);
        return;
    }
    if raw.x == LogicalPx::ZERO && raw.y == LogicalPx::ZERO {
        settle_phase_without_scroll(app, window, phase, session, timestamp);
        return;
    }
    match phase {
        ScrollPhase::Cancel => {
            clear_scroll_session(app, window);
            return;
        }
        ScrollPhase::Begin => session = None,
        ScrollPhase::Discrete | ScrollPhase::Update | ScrollPhase::End | ScrollPhase::Momentum => {}
    }
    let locked = session.filter(|active| session_target_alive(app, window, active.target));
    let target = match locked {
        Some(active) => Some(active.target),
        None => {
            session = None;
            hit.and_then(|hit| {
                app.window(window).and_then(|target_window| {
                    let tree = target_window.tree();
                    let candidates = scroll_candidates(tree, hit);
                    select_target(tree, &candidates, raw, unit)
                })
            })
        }
    };
    let Some(target) = target else {
        clear_scroll_session(app, window);
        return;
    };
    let Some((offset, application)) = app.window(window).and_then(|target_window| {
        let tree = target_window.tree();
        let slot = tree.scroll_slot(target)?;
        let viewport = viewport(tree, target).size;
        let requested = request_px(&slot, raw, unit);
        let application = apply_scroll(&slot, viewport, requested);
        Some((slot.offset, application))
    }) else {
        clear_scroll_session(app, window);
        return;
    };
    if application.moved {
        let goal = LogicalPoint::new(
            offset.x + application.applied.x,
            offset.y + application.applied.y,
        );
        let handle = ScrollHandle::new(window, target);
        let _ = app.scroll_to(&handle, goal);
        refresh_hover_under_pointer(app, window);
    }
    let kind = session.map_or(session_kind(phase), |active| active.kind);
    if phase == ScrollPhase::End && kind == SessionKind::Discrete {
        clear_scroll_session(app, window);
        return;
    }
    let ended_at = match phase {
        ScrollPhase::End => Some(timestamp),
        ScrollPhase::Momentum => session.and_then(|active| active.ended_at),
        ScrollPhase::Begin | ScrollPhase::Update | ScrollPhase::Discrete | ScrollPhase::Cancel => {
            None
        }
    };
    set_scroll_session(
        app,
        window,
        Some(ScrollSession {
            target,
            kind,
            last_event_at: timestamp,
            ended_at,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::{
        ScrollApplication, ancestor_offset, apply_scroll, clamp_offset, max_offset, project_vector,
        request_px, scroll_ancestor, scroll_candidates, select_target, smallest_angle_between,
        to_screen, viewport,
    };
    use crate::{
        event::ScrollUnit,
        interaction::{ScrollAxes, ScrollConfig, ScrollSlot},
        math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
        paint::style::{Clip, Style, StylePatch},
    };

    fn slot(axes: ScrollAxes, content: LogicalSize) -> ScrollSlot {
        ScrollSlot {
            offset: LogicalPoint::ZERO,
            content,
            axes,
            line_height: px(48.0),
        }
    }

    fn slot_at(
        axes: ScrollAxes,
        content: LogicalSize,
        offset: LogicalPoint,
        line_height: f32,
    ) -> ScrollSlot {
        ScrollSlot {
            offset,
            content,
            axes,
            line_height: px(line_height),
        }
    }

    fn attach_slot(tree: &mut NodeTree, node: NodeId, slot: ScrollSlot) {
        tree.ensure_interaction(node).expect("exists").scroll = Some(slot);
    }

    fn clipped_box(tree: &mut NodeTree, parent: NodeId) -> NodeId {
        let mut style = Style::default();
        style.clip = Clip::Bounds;
        NodeBuilder::boxed()
            .style(style)
            .build(tree, parent)
            .expect("mount")
    }

    fn screen() -> LogicalSize {
        LogicalSize::new(px(100.0), px(100.0))
    }

    fn application(slot: &ScrollSlot, requested: LogicalPoint) -> ScrollApplication {
        apply_scroll(slot, screen(), requested)
    }

    #[test]
    fn clamps_both_ends() {
        let content = LogicalSize::new(px(100.0), px(500.0));
        let viewport = LogicalSize::new(px(100.0), px(200.0));
        let mut slot = slot(ScrollAxes::Vertical, content);

        slot.offset = LogicalPoint::new(px(0.0), px(-50.0));
        assert_eq!(clamp_offset(&slot, viewport), LogicalPoint::ZERO);
        slot.offset = LogicalPoint::new(px(0.0), px(10_000.0));
        assert_eq!(clamp_offset(&slot, viewport).y, px(300.0));
        assert_eq!(
            max_offset(&slot, viewport),
            LogicalPoint::new(px(0.0), px(300.0))
        );
    }

    #[test]
    fn zero_range_never_scrolls() {
        let slot = slot(ScrollAxes::Vertical, LogicalSize::new(px(100.0), px(200.0)));
        let viewport = LogicalSize::new(px(100.0), px(200.0));

        assert_eq!(max_offset(&slot, viewport), LogicalPoint::ZERO);
        assert_eq!(clamp_offset(&slot, viewport), LogicalPoint::ZERO);
    }

    #[test]
    fn horizontal_containers_ignore_the_vertical_axis() {
        let mut slot = slot(
            ScrollAxes::Horizontal,
            LogicalSize::new(px(500.0), px(100.0)),
        );
        slot.offset = LogicalPoint::new(px(10_000.0), px(999.0));

        assert_eq!(
            clamp_offset(&slot, LogicalSize::new(px(100.0), px(100.0))),
            LogicalPoint::new(px(400.0), px(0.0))
        );
        assert_eq!(
            max_offset(&slot, LogicalSize::new(px(100.0), px(100.0))),
            LogicalPoint::new(px(400.0), px(0.0))
        );
    }

    #[test]
    fn dual_axis_containers_clamp_each_axis_independently() {
        let mut slot = slot(ScrollAxes::Both, LogicalSize::new(px(500.0), px(500.0)));
        let viewport = LogicalSize::new(px(100.0), px(200.0));
        assert_eq!(
            max_offset(&slot, viewport),
            LogicalPoint::new(px(400.0), px(300.0))
        );

        slot.offset = LogicalPoint::new(px(10_000.0), px(120.0));
        assert_eq!(
            clamp_offset(&slot, viewport),
            LogicalPoint::new(px(400.0), px(120.0)),
            "一轴到边界不影响另一轴"
        );
        slot.offset = LogicalPoint::new(px(-50.0), px(-50.0));
        assert_eq!(clamp_offset(&slot, viewport), LogicalPoint::ZERO);
    }

    #[test]
    fn disabled_axis_stays_zero_even_with_overflowing_content() {
        let mut slot = slot(
            ScrollAxes::Vertical,
            LogicalSize::new(px(5_000.0), px(5_000.0)),
        );
        slot.offset = LogicalPoint::new(px(321.0), px(222.0));
        let viewport = LogicalSize::new(px(100.0), px(200.0));

        assert_eq!(max_offset(&slot, viewport).x, LogicalPx::ZERO);
        assert_eq!(clamp_offset(&slot, viewport).x, LogicalPx::ZERO);
        assert_eq!(
            clamp_offset(&slot, viewport),
            LogicalPoint::new(px(0.0), px(222.0)),
            "未启用轴恒 0、启用轴照常夹紧"
        );
    }

    #[test]
    fn ancestor_offset_excludes_the_node_itself() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = clipped_box(&mut tree, root);
        let inner = clipped_box(&mut tree, outer);
        tree.ensure_interaction(outer).expect("exists").scroll = Some(ScrollSlot {
            offset: LogicalPoint::new(px(0.0), px(30.0)),
            content: LogicalSize::new(px(100.0), px(400.0)),
            axes: ScrollAxes::Vertical,
            line_height: px(48.0),
        });
        tree.ensure_interaction(inner).expect("exists").scroll = Some(ScrollSlot {
            offset: LogicalPoint::new(px(0.0), px(10.0)),
            content: LogicalSize::new(px(100.0), px(400.0)),
            axes: ScrollAxes::Vertical,
            line_height: px(48.0),
        });

        assert_eq!(
            ancestor_offset(&tree, outer),
            LogicalPoint::ZERO,
            "自身 offset 不算"
        );
        assert_eq!(
            ancestor_offset(&tree, inner),
            LogicalPoint::new(px(0.0), px(30.0))
        );
        let rect = LogicalRect::new(px(0.0), px(100.0), px(50.0), px(20.0));
        assert_eq!(
            to_screen(&tree, inner, rect).origin,
            LogicalPoint::new(px(0.0), px(70.0))
        );
    }

    #[test]
    fn viewport_is_the_padding_box() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let node = clipped_box(&mut tree, root);
        let bounds = crate::math::LogicalRect::new(px(10.0), px(20.0), px(100.0), px(80.0));
        let border = crate::math::LogicalEdges::uniform(px(2.0));
        tree.set_layout_for_test(node, crate::node::tree::LayoutBox { bounds, border });

        assert_eq!(
            viewport(&tree, node),
            crate::math::LogicalRect::new(px(12.0), px(22.0), px(96.0), px(76.0))
        );
    }

    #[test]
    fn nearest_scroll_ancestor_wins() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = clipped_box(&mut tree, root);
        let inner = clipped_box(&mut tree, outer);
        let leaf = NodeBuilder::boxed().build(&mut tree, inner).expect("mount");
        let config = ScrollConfig::default();
        for node in [outer, inner] {
            tree.ensure_interaction(node).expect("exists").scroll = Some(ScrollSlot {
                offset: LogicalPoint::ZERO,
                content: LogicalSize::new(px(100.0), px(400.0)),
                axes: config.axes,
                line_height: config.line_height,
            });
        }

        assert_eq!(
            scroll_ancestor(&tree, leaf).map(|(node, _)| node),
            Some(inner)
        );
        assert_eq!(scroll_ancestor(&tree, root), None);
    }

    #[test]
    fn smallest_angle_and_projection_match_chromium() {
        let vertical = slot_at(
            ScrollAxes::Vertical,
            LogicalSize::new(px(100.0), px(600.0)),
            LogicalPoint::ZERO,
            48.0,
        );
        let requested = request_px(
            &vertical,
            LogicalPoint::new(px(0.0), px(-3.0)),
            ScrollUnit::Line,
        );
        assert_eq!(
            requested,
            LogicalPoint::new(px(0.0), px(144.0)),
            "E34a：行滚轮按目标行高换算并归一化到 offset 增加方向"
        );
        let e34a = application(&vertical, requested);
        assert_eq!(e34a.applied, LogicalPoint::new(px(0.0), px(144.0)));
        assert_eq!(e34a.consumed, requested);
        assert!(e34a.moved);

        let e34b = application(&vertical, LogicalPoint::new(px(12.0), px(3.0)));
        assert_eq!(e34b.applied, LogicalPoint::new(px(0.0), px(3.0)));
        let angle = smallest_angle_between(e34b.applied, LogicalPoint::new(px(12.0), px(3.0)));
        assert!((angle - 75.96).abs() < 0.01, "E34b angle={angle}");
        assert_eq!(e34b.consumed, LogicalPoint::new(px(0.0), px(3.0)));

        let e34c = application(&vertical, LogicalPoint::new(px(2.0), px(3.0)));
        assert_eq!(e34c.applied, LogicalPoint::new(px(0.0), px(3.0)));
        let angle = smallest_angle_between(e34c.applied, LogicalPoint::new(px(2.0), px(3.0)));
        assert!((angle - 33.69).abs() < 0.01, "E34c angle={angle}");
        assert_eq!(e34c.consumed, LogicalPoint::new(px(2.0), px(3.0)));

        let e34d = application(&vertical, LogicalPoint::new(px(3.0), px(3.0)));
        assert_eq!(e34d.applied, LogicalPoint::new(px(0.0), px(3.0)));
        assert_eq!(
            smallest_angle_between(e34d.applied, LogicalPoint::new(px(3.0), px(3.0))),
            45.0,
            "E34d：严格 45° 边界"
        );
        assert_eq!(
            e34d.consumed,
            LogicalPoint::new(px(0.0), px(3.0)),
            "E34d：45° 走投影而不是整段吞掉"
        );

        let e34e = application(&vertical, LogicalPoint::ZERO);
        assert_eq!(e34e.consumed, LogicalPoint::ZERO);
        assert!(!e34e.moved);

        let e34f = application(&vertical, LogicalPoint::new(px(30.0), px(0.0)));
        assert_eq!(e34f.applied, LogicalPoint::ZERO);
        assert_eq!(e34f.consumed, LogicalPoint::ZERO);
        assert!(!e34f.moved);

        let pinned = slot_at(
            ScrollAxes::Both,
            LogicalSize::new(px(500.0), px(600.0)),
            LogicalPoint::new(px(400.0), px(0.0)),
            48.0,
        );
        let e34g = application(&pinned, LogicalPoint::new(px(10.0), px(100.0)));
        assert_eq!(e34g.applied, LogicalPoint::new(px(0.0), px(100.0)));
        let angle = smallest_angle_between(e34g.applied, LogicalPoint::new(px(10.0), px(100.0)));
        assert!((angle - 5.71).abs() < 0.01, "E34g angle={angle}");
        assert_eq!(
            e34g.consumed,
            LogicalPoint::new(px(10.0), px(100.0)),
            "E34g：小于 45° 整段吞掉（含未滚动轴）"
        );

        let e34h = application(&pinned, LogicalPoint::new(px(80.0), px(100.0)));
        assert_eq!(e34h.applied, LogicalPoint::new(px(0.0), px(100.0)));
        let angle = smallest_angle_between(e34h.applied, LogicalPoint::new(px(80.0), px(100.0)));
        assert!((angle - 38.66).abs() < 0.01, "E34h angle={angle}");
        assert_eq!(e34h.consumed, LogicalPoint::new(px(80.0), px(100.0)));

        let e34i = application(&pinned, LogicalPoint::new(px(120.0), px(100.0)));
        assert_eq!(e34i.applied, LogicalPoint::new(px(0.0), px(100.0)));
        let angle = smallest_angle_between(e34i.applied, LogicalPoint::new(px(120.0), px(100.0)));
        assert!((angle - 50.19).abs() < 0.01, "E34i angle={angle}");
        assert_eq!(
            e34i.consumed,
            LogicalPoint::new(px(0.0), px(100.0)),
            "E34i：大于 45° 投影到已滚方向"
        );

        let free = slot_at(
            ScrollAxes::Both,
            LogicalSize::new(px(500.0), px(600.0)),
            LogicalPoint::ZERO,
            48.0,
        );
        let e34j = application(&free, LogicalPoint::new(px(0.0), px(100.0)));
        assert_eq!(e34j.applied, LogicalPoint::new(px(0.0), px(100.0)));
        assert_eq!(e34j.consumed, LogicalPoint::new(px(0.0), px(100.0)));

        let horizontal_disabled = application(&vertical, LogicalPoint::new(px(-100.0), px(10.0)));
        assert_eq!(
            horizontal_disabled.applied,
            LogicalPoint::new(px(0.0), px(10.0)),
            "E34k：未启用轴的 applied 恒 0"
        );
        let angle = smallest_angle_between(
            horizontal_disabled.applied,
            LogicalPoint::new(px(-100.0), px(10.0)),
        );
        assert!((angle - 84.29).abs() < 0.01, "E34k angle={angle}");
        assert_eq!(
            horizontal_disabled.consumed,
            LogicalPoint::new(px(0.0), px(10.0)),
            "E34k：请求向量保留未启用轴分量参与角度比较"
        );

        let no_range = slot_at(
            ScrollAxes::Vertical,
            LogicalSize::new(px(100.0), px(100.0)),
            LogicalPoint::ZERO,
            48.0,
        );
        let e34l = application(&no_range, LogicalPoint::new(px(5.0), px(0.0)));
        assert_eq!(e34l.applied, LogicalPoint::ZERO);
        assert_eq!(e34l.consumed, LogicalPoint::ZERO);
        assert!(!e34l.moved);
    }

    #[test]
    fn apply_scroll_consumes_whole_delta_within_45_degrees() {
        let slot = slot_at(
            ScrollAxes::Vertical,
            LogicalSize::new(px(100.0), px(600.0)),
            LogicalPoint::ZERO,
            48.0,
        );
        let application = self::application(&slot, LogicalPoint::new(px(2.0), px(3.0)));

        assert_eq!(application.applied, LogicalPoint::new(px(0.0), px(3.0)));
        assert_eq!(application.consumed, LogicalPoint::new(px(2.0), px(3.0)));
        assert!(application.moved);
    }

    #[test]
    fn apply_scroll_projects_beyond_45_degrees() {
        let slot = slot_at(
            ScrollAxes::Vertical,
            LogicalSize::new(px(100.0), px(600.0)),
            LogicalPoint::ZERO,
            48.0,
        );
        let application = self::application(&slot, LogicalPoint::new(px(12.0), px(3.0)));

        assert_eq!(application.applied, LogicalPoint::new(px(0.0), px(3.0)));
        assert_eq!(application.consumed, LogicalPoint::new(px(0.0), px(3.0)));
    }

    #[test]
    fn zero_or_blocked_delta_consumes_nothing() {
        let slot = slot_at(
            ScrollAxes::Vertical,
            LogicalSize::new(px(100.0), px(600.0)),
            LogicalPoint::ZERO,
            48.0,
        );
        for requested in [LogicalPoint::ZERO, LogicalPoint::new(px(30.0), px(0.0))] {
            let application = self::application(&slot, requested);
            assert!(!application.moved);
            assert_eq!(application.applied, LogicalPoint::ZERO);
            assert_eq!(application.consumed, LogicalPoint::ZERO);
        }
        assert_eq!(
            project_vector(LogicalPoint::new(px(3.0), px(4.0)), LogicalPoint::ZERO),
            LogicalPoint::new(px(3.0), px(4.0)),
            "零向量投影返回 source"
        );
        assert_eq!(
            smallest_angle_between(LogicalPoint::ZERO, LogicalPoint::new(px(1.0), px(0.0))),
            0.0,
            "零向量角度定义为 0"
        );
        assert_eq!(
            project_vector(
                LogicalPoint::new(px(12.0), px(3.0)),
                LogicalPoint::new(px(0.0), px(3.0))
            ),
            LogicalPoint::new(px(0.0), px(3.0))
        );
    }

    #[test]
    fn target_selection_converts_lines_per_candidate() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let bounds = LogicalRect::new(px(0.0), px(0.0), px(200.0), px(100.0));
        let outer = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            outer,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let inner = clipped_box(&mut tree, outer);
        attach_slot(
            &mut tree,
            inner,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                20.0,
            ),
        );
        let leaf = NodeBuilder::boxed().build(&mut tree, inner).expect("mount");
        for node in [outer, inner] {
            tree.set_layout_for_test(
                node,
                crate::node::tree::LayoutBox {
                    bounds,
                    border: crate::math::LogicalEdges::uniform(px(0.0)),
                },
            );
        }
        let candidates = scroll_candidates(&tree, leaf);

        assert_eq!(candidates.as_slice(), &[inner, outer]);
        assert_eq!(
            select_target(
                &tree,
                &candidates,
                LogicalPoint::new(px(0.0), px(-3.0)),
                ScrollUnit::Line
            ),
            Some(inner),
            "E34m：逐候选用自己的 line_height 换算（20），不得用全局 48"
        );
        let inner_slot = tree.scroll_slot(inner).expect("inner slot");
        let requested = request_px(
            &inner_slot,
            LogicalPoint::new(px(0.0), px(-3.0)),
            ScrollUnit::Line,
        );
        assert_eq!(requested, LogicalPoint::new(px(0.0), px(60.0)));
    }

    #[test]
    fn target_selection_continues_when_small_line_height_cannot_consume() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let bounds = LogicalRect::new(px(0.0), px(0.0), px(200.0), px(100.0));
        let outer = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            outer,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let inner = clipped_box(&mut tree, outer);
        attach_slot(
            &mut tree,
            inner,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(100.0)),
                LogicalPoint::ZERO,
                20.0,
            ),
        );
        let leaf = NodeBuilder::boxed().build(&mut tree, inner).expect("mount");
        for node in [outer, inner] {
            tree.set_layout_for_test(
                node,
                crate::node::tree::LayoutBox {
                    bounds,
                    border: crate::math::LogicalEdges::uniform(px(0.0)),
                },
            );
        }
        let candidates = scroll_candidates(&tree, leaf);

        assert_eq!(
            select_target(
                &tree,
                &candidates,
                LogicalPoint::new(px(0.0), px(-3.0)),
                ScrollUnit::Line
            ),
            Some(outer),
            "E34n：内层行高 20 不可消费时继续到外层（行高 48）"
        );
    }

    #[test]
    fn target_selection_skips_hidden_or_disabled_candidates() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let outer = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            outer,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let inner = clipped_box(&mut tree, outer);
        attach_slot(
            &mut tree,
            inner,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let leaf = NodeBuilder::boxed().build(&mut tree, inner).expect("mount");
        let raw = LogicalPoint::new(px(0.0), px(-1.0));

        tree.set_style_patch(
            inner,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );
        assert_eq!(
            select_target(
                &tree,
                &scroll_candidates(&tree, leaf),
                raw,
                ScrollUnit::Line
            ),
            Some(outer),
            "隐藏候选必须跳过"
        );

        tree.set_style_patch(
            inner,
            StylePatch {
                visible: None,
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        assert_eq!(
            select_target(
                &tree,
                &scroll_candidates(&tree, leaf),
                raw,
                ScrollUnit::Line
            ),
            Some(outer),
            "禁用候选必须跳过"
        );
    }

    #[test]
    fn target_selection_skips_a_proxy_target_behind_a_hidden_ancestor() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let page = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            page,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let hidden_layer = NodeBuilder::boxed().build(&mut tree, page).expect("mount");
        let pane = clipped_box(&mut tree, hidden_layer);
        attach_slot(
            &mut tree,
            pane,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let track = NodeBuilder::boxed().build(&mut tree, page).expect("mount");
        tree.set_scroll_proxy(track, Some(pane));
        tree.set_style_patch(
            hidden_layer,
            StylePatch {
                visible: Some(false),
                ..StylePatch::default()
            },
        );

        assert!(
            tree.is_visible_locally(pane),
            "代理目标自身可见，但祖先隐藏"
        );
        assert!(!tree.is_effectively_visible(pane));
        assert_eq!(
            select_target(
                &tree,
                &scroll_candidates(&tree, track),
                LogicalPoint::new(px(0.0), px(-1.0)),
                ScrollUnit::Line
            ),
            Some(page),
            "隐藏祖先下的代理目标必须跳过并继续到可见候选"
        );
    }

    #[test]
    fn proxy_chains_follow_every_hop_to_the_terminal_container() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let page = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            page,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let mut chained = Vec::new();
        for _ in 0..3 {
            let node = NodeBuilder::boxed().build(&mut tree, page).expect("mount");
            attach_slot(
                &mut tree,
                node,
                slot_at(
                    ScrollAxes::Vertical,
                    LogicalSize::new(px(200.0), px(600.0)),
                    LogicalPoint::ZERO,
                    48.0,
                ),
            );
            chained.push(node);
        }
        tree.set_scroll_proxy(chained[0], Some(chained[1]));
        tree.set_scroll_proxy(chained[1], Some(chained[2]));

        assert_eq!(
            scroll_candidates(&tree, chained[0]).as_slice(),
            &[chained[2], page],
            "A→B→C 链只产出最终容器 C，并从 C 的父链继续；中间节点 B 不产出"
        );
    }

    #[test]
    fn invalid_proxy_hop_falls_back_to_the_node_and_its_parent_chain() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let page = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            page,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let node = NodeBuilder::boxed().build(&mut tree, page).expect("mount");
        attach_slot(
            &mut tree,
            node,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let bare = NodeBuilder::boxed().build(&mut tree, page).expect("mount");
        tree.set_scroll_proxy(node, Some(bare));

        assert_eq!(
            scroll_candidates(&tree, node).as_slice(),
            &[node, page],
            "代理跳失效时退化为无代理：产出自身槽并从自身父链继续"
        );
    }

    #[test]
    fn scroll_candidates_follow_the_proxy_before_the_node_itself() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let page = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            page,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let pane = clipped_box(&mut tree, page);
        attach_slot(
            &mut tree,
            pane,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let track = NodeBuilder::boxed().build(&mut tree, page).expect("mount");
        attach_slot(
            &mut tree,
            track,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let thumb = NodeBuilder::boxed().build(&mut tree, track).expect("mount");
        assert!(tree.set_scroll_proxy(track, Some(pane)));

        assert_eq!(
            scroll_candidates(&tree, track).as_slice(),
            &[pane, page],
            "代理优先于自身，并从代理目标的父节点继续"
        );
        assert_eq!(
            scroll_candidates(&tree, thumb).as_slice(),
            &[pane, page],
            "滚动条 thumb 经 track 的代理归属目标容器"
        );
        assert_eq!(tree.scroll_proxy(track), Some(pane));

        tree.set_scroll_proxy(track, None);
        assert_eq!(
            scroll_candidates(&tree, track).as_slice(),
            &[track, page],
            "清除代理后回到自身"
        );
    }

    #[test]
    fn scroll_candidates_drop_an_invalid_proxy_and_walk_to_the_parent() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let page = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            page,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let track = NodeBuilder::boxed().build(&mut tree, page).expect("mount");
        let bare = NodeBuilder::boxed().build(&mut tree, page).expect("mount");
        tree.set_scroll_proxy(track, Some(bare));

        assert_eq!(
            scroll_candidates(&tree, track).as_slice(),
            &[page],
            "代理目标没有滚动槽时按无效代理跳过并继续父链"
        );

        let pane = clipped_box(&mut tree, page);
        attach_slot(
            &mut tree,
            pane,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        tree.set_scroll_proxy(track, Some(pane));
        assert!(tree.remove(pane).is_ok());
        assert_eq!(
            scroll_candidates(&tree, track).as_slice(),
            &[page],
            "代理目标卸载后不得产出死节点"
        );
    }

    #[test]
    fn ancestor_to_descendant_proxy_cycles_terminate() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let ancestor = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            ancestor,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let descendant = clipped_box(&mut tree, ancestor);
        attach_slot(
            &mut tree,
            descendant,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        tree.set_scroll_proxy(ancestor, Some(descendant));

        assert_eq!(
            scroll_candidates(&tree, ancestor).as_slice(),
            &[descendant],
            "祖先指向后代的代理环必须终止且只产出目标一次"
        );
    }

    #[test]
    fn proxy_jumps_terminate_on_a_cross_subtree_cycle() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let left = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            left,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let inner_left = clipped_box(&mut tree, left);
        attach_slot(
            &mut tree,
            inner_left,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let right = clipped_box(&mut tree, root);
        attach_slot(
            &mut tree,
            right,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let inner_right = clipped_box(&mut tree, right);
        attach_slot(
            &mut tree,
            inner_right,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        tree.set_scroll_proxy(left, Some(inner_right));
        tree.set_scroll_proxy(right, Some(inner_left));

        assert_eq!(
            scroll_candidates(&tree, left).as_slice(),
            &[inner_right, inner_left],
            "跨子树代理跳转后由 visited 终止，不重复产出"
        );
    }

    #[test]
    fn deep_candidate_chains_stop_at_the_hop_limit() {
        let mut tree = NodeTree::new();
        let root = tree.root();
        let mut nodes = vec![clipped_box(&mut tree, root)];
        for _ in 1..40 {
            let parent = *nodes.last().expect("parent");
            nodes.push(clipped_box(&mut tree, parent));
        }
        attach_slot(
            &mut tree,
            nodes[0],
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        let deepest = *nodes.last().expect("deepest");
        assert!(
            scroll_candidates(&tree, deepest).is_empty(),
            "超过 MAX_CHAIN_HOPS 的祖先不得进入候选链"
        );

        let reachable = nodes[nodes.len() - 31];
        attach_slot(
            &mut tree,
            reachable,
            slot_at(
                ScrollAxes::Vertical,
                LogicalSize::new(px(200.0), px(600.0)),
                LogicalPoint::ZERO,
                48.0,
            ),
        );
        assert_eq!(
            scroll_candidates(&tree, deepest).as_slice(),
            &[reachable],
            "安全阀内的深链仍可解析"
        );
    }
}
