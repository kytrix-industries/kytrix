use smallvec::SmallVec;

use crate::{
    app::App,
    error::EngineError,
    layout::LayoutReport,
    math::{LogicalPoint, LogicalRect, LogicalSize, px},
    node::{id::NodeId, tree::MaxSize},
    popup::{
        Anchor, PlacementInput, PopupDiagnostic, PopupEntry, PopupId, PopupState, PopupTier,
        SettleOutcome,
    },
    text::TextLayoutReport,
    window::WindowId,
};

const MAX_SETTLE_TEXT_PASSES: usize = 4;

fn merge_layout(target: &mut Option<LayoutReport>, report: LayoutReport) {
    match target {
        Some(existing) => {
            existing.laid_out += report.laid_out;
            existing.moved += report.moved;
            existing.cache_hits += report.cache_hits;
            existing.cache_misses += report.cache_misses;
            existing.bounds_updated += report.bounds_updated;
        }
        None => *target = Some(report),
    }
}

fn merge_text_layout(target: &mut Option<TextLayoutReport>, report: TextLayoutReport) {
    match target {
        Some(existing) => {
            existing.laid += report.laid;
            existing.reused += report.reused;
            existing.reruns += report.reruns;
            existing.rasterized += report.rasterized;
            existing.tiles_reused += report.tiles_reused;
            existing.atlas_rebuilds += report.atlas_rebuilds;
            existing.atlas_evictions += report.atlas_evictions;
            existing.skipped_glyphs += report.skipped_glyphs;
            existing.skipped_color_glyphs += report.skipped_color_glyphs;
        }
        None => *target = Some(report),
    }
}

fn relayout(
    app: &mut App,
    window: WindowId,
    layout: &mut Option<LayoutReport>,
    text_layout: &mut Option<TextLayoutReport>,
) -> Result<(), EngineError> {
    match app
        .window_mut(window)
        .map(crate::window::Window::layout_if_dirty)
    {
        Some(Ok(Some(report))) => merge_layout(layout, report),
        Some(Ok(None)) => {}
        Some(Err(error)) => return Err(error),
        None => return Err(EngineError::UnknownWindow { window }),
    }
    let mut rounds = 0usize;
    loop {
        match app.layout_text(window) {
            Some(Ok(report)) => merge_text_layout(text_layout, report),
            Some(Err(error)) => return Err(error),
            None => return Err(EngineError::UnknownWindow { window }),
        }
        app.rewindow_virtuals(window);
        if app.clamp_scroll_offsets(window) {
            app.request_frame();
        }
        let Some(Ok(resolution)) = app.resolve_canvas_text(window) else {
            return Err(EngineError::UnknownWindow { window });
        };
        if !resolution.evicted || rounds >= MAX_SETTLE_TEXT_PASSES {
            break;
        }
        rounds += 1;
    }
    Ok(())
}

fn max_size_for(entry: &PopupEntry, viewport: LogicalSize) -> MaxSize {
    let margin = entry.placement.viewport_margin;
    let available_width = px((viewport.width.value() - 2.0 * margin.value()).max(0.0));
    let available_height = px((viewport.height.value() - 2.0 * margin.value()).max(0.0));
    let max = entry.placement.max_size;
    MaxSize {
        width: max.shrinks_width().then_some(available_width),
        height: max.shrinks_height().then_some(available_height),
    }
}

fn layout_rect(tree: &crate::node::tree::NodeTree, node: NodeId) -> LogicalRect {
    tree.get(node)
        .map_or(LogicalRect::default(), |entry| entry.layout().bounds)
}

struct Solved {
    id: PopupId,
    origin: LogicalPoint,
    size: LogicalSize,
    delta: LogicalPoint,
    resized: bool,
}

pub(crate) fn settle(
    app: &mut App,
    window: WindowId,
    layout_ran: bool,
) -> Result<Option<SettleOutcome>, EngineError> {
    let (needs, ids, viewport, order_dirty) = {
        let Some(target) = app.window(window) else {
            return Ok(None);
        };
        let stack = target.popups();
        let viewport = target.size();
        let viewport_changed = stack.viewport() != viewport;
        let needs = stack.dirty().any()
            || viewport_changed
            || (layout_ran && !stack.is_empty())
            || stack.animating();
        let ids = stack.depth_ordered();
        let order_dirty = stack.dirty().order;
        (needs, ids, viewport, order_dirty)
    };
    if !needs {
        return Ok(None);
    }
    if ids.is_empty() {
        let Some(target) = app.window_mut(window) else {
            return Ok(None);
        };
        let (stack, tree) = target.popups_and_tree();
        stack.set_viewport(viewport);
        stack.take_dirty();
        stack.clear_diagnostics();
        let _ = tree;
        return Ok(Some(SettleOutcome {
            rounds: 0,
            layout: None,
            text_layout: None,
            unsettled: false,
        }));
    }
    {
        let Some(target) = app.window_mut(window) else {
            return Ok(None);
        };
        let (stack, tree) = target.popups_and_tree();
        stack.set_viewport(viewport);
        stack.take_dirty();
        stack.clear_diagnostics();
        if order_dirty {
            stack.sync_order(tree)?;
        }
    }
    let mut outcome = SettleOutcome {
        rounds: 0,
        layout: None,
        text_layout: None,
        unsettled: false,
    };
    let mut pre_max_sizes: SmallVec<[(PopupId, LogicalSize); 8]> =
        SmallVec::with_capacity(ids.len());
    for id in ids.iter().copied() {
        let Some(target) = app.window(window) else {
            break;
        };
        let Ok(entry) = target.popups().entry(id) else {
            continue;
        };
        if entry.tier != PopupTier::Scrim
            && matches!(entry.state, PopupState::Opening | PopupState::Open)
        {
            pre_max_sizes.push((id, layout_rect(target.tree(), entry.node).size));
        }
    }
    let mut max_changed = false;
    let mut scrim_changed = false;
    for id in ids.iter().copied() {
        let Some(target) = app.window_mut(window) else {
            break;
        };
        let Ok(entry) = target.popups().entry(id) else {
            continue;
        };
        if entry.tier == PopupTier::Scrim {
            let node = entry.node;
            let changed = target.tree_mut().set_absolute_size(node, viewport);
            scrim_changed |= changed;
            continue;
        }
        let node = entry.node;
        let max = max_size_for(entry, viewport);
        if target.popups().max_size_of(id) == Some(max) {
            continue;
        }
        if target.tree_mut().set_max_size(node, max).is_ok() {
            target.popups_mut().set_max_size(id, max);
            max_changed = true;
        }
    }
    if max_changed || scrim_changed {
        outcome.rounds += 1;
        relayout(app, window, &mut outcome.layout, &mut outcome.text_layout)?;
    }
    let mut pre_placement_sizes: SmallVec<[(PopupId, LogicalSize); 8]> =
        SmallVec::with_capacity(ids.len());
    for id in ids.iter().copied() {
        let Some(target) = app.window(window) else {
            break;
        };
        let Ok(entry) = target.popups().entry(id) else {
            continue;
        };
        if !matches!(entry.state, PopupState::Opening | PopupState::Open)
            || entry.tier == PopupTier::Scrim
        {
            continue;
        }
        pre_placement_sizes.push((id, layout_rect(target.tree(), entry.node).size));
    }
    let mut solved: SmallVec<[Solved; 8]> = SmallVec::with_capacity(ids.len());
    let mut dismissals: SmallVec<[PopupId; 4]> = SmallVec::new();
    let mut diagnostics: SmallVec<[PopupDiagnostic; 4]> = SmallVec::new();
    for id in ids.iter().copied() {
        let Some(target) = app.window(window) else {
            break;
        };
        let stack = target.popups();
        let tree = target.tree();
        let Ok(entry) = stack.entry(id) else {
            continue;
        };
        if !matches!(entry.state, PopupState::Opening | PopupState::Open) {
            continue;
        }
        if entry.tier == PopupTier::Scrim {
            solved.push(Solved {
                id,
                origin: LogicalPoint::ZERO,
                size: viewport,
                delta: LogicalPoint::ZERO,
                resized: false,
            });
            continue;
        }
        let node = entry.node;
        let current = layout_rect(tree, node);
        let parent = entry.parent;
        let parent_delta = parent
            .and_then(|parent| solved.iter().find(|solved| solved.id == parent))
            .map_or(LogicalPoint::ZERO, |solved| solved.delta);
        let anchor = match resolve_anchor(tree, entry.anchor) {
            Ok(anchor) => LogicalRect::from_origin_size(anchor.origin + parent_delta, anchor.size),
            Err(crate::popup::PopupError::AnchorNodeMissing { .. }) => {
                dismissals.push(id);
                continue;
            }
            Err(_) => {
                outcome.unsettled = true;
                diagnostics.push(PopupDiagnostic::Unsettled { id });
                continue;
            }
        };
        let avoid = parent.and_then(|parent| {
            solved
                .iter()
                .find(|solved| solved.id == parent)
                .map(|solved| LogicalRect::from_origin_size(solved.origin, solved.size))
        });
        let input = PlacementInput {
            anchor,
            size: current.size,
            viewport,
            avoid,
            spec: entry.placement.clone(),
        };
        let Ok(result) = crate::popup::placement::solve_placement(input) else {
            outcome.unsettled = true;
            diagnostics.push(PopupDiagnostic::Unsettled { id });
            continue;
        };
        let delta = LogicalPoint::new(
            result.rect.origin.x - current.origin.x,
            result.rect.origin.y - current.origin.y,
        );
        solved.push(Solved {
            id,
            origin: result.rect.origin,
            size: result.rect.size,
            delta,
            resized: result.resized,
        });
        if result.resized {
            diagnostics.push(PopupDiagnostic::Resized {
                id,
                size: result.rect.size,
            });
        }
    }
    if !diagnostics.is_empty()
        && let Some(target) = app.window_mut(window)
    {
        for diagnostic in diagnostics {
            target.popups_mut().push_diagnostic(diagnostic);
        }
    }
    let mut insets_changed = false;
    {
        let Some(target) = app.window_mut(window) else {
            return Ok(Some(outcome));
        };
        let (stack, tree) = target.popups_and_tree();
        for current in &solved {
            let Ok(entry) = stack.entry(current.id) else {
                continue;
            };
            let node = entry.node;
            let live = layout_rect(tree, node);
            if entry.tier == PopupTier::Scrim {
                if tree.set_absolute_size(node, viewport) {
                    insets_changed = true;
                }
                if live.origin != LogicalPoint::ZERO
                    && tree.set_absolute_inset(node, LogicalPoint::ZERO)
                {
                    insets_changed = true;
                }
                continue;
            }
            if live.origin != current.origin {
                let _ = tree.set_absolute_inset(node, current.origin);
                insets_changed = true;
            }
        }
    }
    if insets_changed {
        outcome.rounds += 1;
        relayout(app, window, &mut outcome.layout, &mut outcome.text_layout)?;
    }
    {
        let Some(target) = app.window_mut(window) else {
            return Ok(Some(outcome));
        };
        let (stack, tree) = target.popups_and_tree();
        stack.sync_rects(tree);
        for current in &solved {
            let Some((node, pivot)) = stack
                .entry(current.id)
                .ok()
                .map(|entry| (entry.node, entry.paint_center()))
            else {
                continue;
            };
            let Some(mut transform) = tree.popup_transform(node) else {
                continue;
            };
            if transform.scale != 1.0 && transform.pivot != pivot {
                transform.pivot = pivot;
                tree.set_popup_transform(node, Some(transform));
            }
        }
        for (id, before) in pre_placement_sizes {
            let Ok(entry) = stack.entry(id) else {
                continue;
            };
            let after = layout_rect(tree, entry.node).size;
            if after != before {
                outcome.unsettled = true;
                stack.push_diagnostic(PopupDiagnostic::Unsettled { id });
            }
        }
        if outcome.unsettled {
            stack.mark(crate::popup::PopupDirty {
                geometry: true,
                order: false,
            });
        } else {
            for current in &solved {
                let _ = stack.set_state(current.id, PopupState::Open);
            }
            let mut constrained: SmallVec<[(PopupId, LogicalSize); 8]> = SmallVec::new();
            for (id, before) in pre_max_sizes {
                let Ok(entry) = stack.entry(id) else {
                    continue;
                };
                let after = layout_rect(tree, entry.node).size;
                let resized = solved
                    .iter()
                    .any(|current| current.id == id && current.resized);
                let changed = (after.width.value() - before.width.value()).abs() > 0.01
                    || (after.height.value() - before.height.value()).abs() > 0.01;
                if changed || resized {
                    constrained.push((id, after));
                }
                if let Ok(entry) = stack.entry_mut(id) {
                    entry.constrained_size = (changed || resized).then_some(after);
                }
            }
            for (id, size) in constrained {
                stack.push_diagnostic(PopupDiagnostic::Resized { id, size });
            }
        }
    }
    for id in dismissals {
        crate::popup::dismiss::anchor_lost(app, window, id);
    }
    if outcome.unsettled {
        app.request_frame();
    }
    #[cfg(feature = "test-support")]
    if let Some(target) = app.window_mut(window) {
        target.set_popup_settle_rounds(outcome.rounds);
    }
    Ok(Some(outcome))
}

fn resolve_anchor(
    tree: &crate::node::tree::NodeTree,
    anchor: Anchor,
) -> Result<LogicalRect, crate::popup::PopupError> {
    crate::popup::anchor::resolve_anchor(tree, anchor)
}
