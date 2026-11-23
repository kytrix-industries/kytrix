use accesskit::{Action, ActionData, ActionRequest};

use crate::{
    a11y::{
        A11yAction, A11yActionCtx, A11yActionData, A11yDiagnostic, A11yNodeId, A11yRole,
        A11yTextPosition,
        mapping::{action_from_accesskit, effective_role_of},
        textrun,
    },
    app::App,
    event::ActionSource,
    interaction::focus::{action_target, focusable, in_scope},
    math::{LogicalPoint, LogicalPx, px},
    node::{
        id::NodeId,
        tree::{Node, NodeKind, NodeTree},
    },
    window::{Window, WindowId},
};

pub(crate) fn dispatch(app: &mut App, window: WindowId, request: ActionRequest) {
    if request.target_tree != accesskit::TreeId::ROOT {
        return;
    }
    let target_id = A11yNodeId::from_stable(request.target_node.0);
    let Some(engine) = app
        .window(window)
        .and_then(|target| target.a11y_engine_of(target_id))
    else {
        note_stale(app, request.action);
        return;
    };
    if app
        .window(window)
        .is_none_or(|target| target.tree().get(engine).is_err())
    {
        note_stale(app, request.action);
        return;
    }

    let scope = app.modal_scope(window);
    let allowed = app
        .window(window)
        .is_some_and(|target| in_scope(target.tree(), engine, scope));
    if !allowed {
        app.a11y_note(A11yDiagnostic::UnhandledAction {
            node: engine,
            action: action_from_accesskit(request.action),
        });
        return;
    }

    let data = request.data;
    if request.action == Action::Blur {
        if app.focused(window) == Some(engine) {
            app.set_focus(window, None);
        }
        return;
    }
    let action = match (request.action, data.as_ref()) {
        (Action::CustomAction, Some(ActionData::CustomAction(id))) => A11yAction::Custom(*id),
        (Action::ReplaceSelectedText, _) => A11yAction::SetValue,
        (action, _) => match action_from_accesskit(action) {
            Some(action) => action,
            None => {
                app.a11y_note(A11yDiagnostic::UnhandledAction {
                    node: engine,
                    action: None,
                });
                return;
            }
        },
    };

    let disabled = app
        .window(window)
        .is_some_and(|target| crate::a11y::mapping::a11y_disabled(target.tree(), engine));
    if disabled && is_state_changing(action) {
        app.a11y_note(A11yDiagnostic::DisabledAction {
            node: engine,
            action,
        });
        return;
    }

    let (allowed, read_only) = app
        .window(window)
        .map(|target| {
            let tree = target.tree();
            (
                crate::a11y::mapping::action_allowed(tree, engine, action),
                crate::a11y::mapping::a11y_read_only(tree, engine),
            )
        })
        .unwrap_or((false, false));
    if !allowed {
        let diagnostic = if read_only && crate::a11y::mapping::is_value_mutation(action) {
            A11yDiagnostic::ReadOnlyAction {
                node: engine,
                action,
            }
        } else {
            A11yDiagnostic::UnadvertisedAction {
                node: engine,
                action,
            }
        };
        app.a11y_note(diagnostic);
        return;
    }

    let mut handler_node = engine;
    let payload = match action {
        A11yAction::Increment | A11yAction::Decrement => {
            increment_payload(app, window, engine, action, data.as_ref())
        }
        A11yAction::SetTextSelection => {
            let Some((target, payload)) = selection_payload(app, window, engine, data.as_ref())
            else {
                app.a11y_note(A11yDiagnostic::InvalidActionData {
                    node: engine,
                    action,
                });
                return;
            };
            handler_node = target;
            payload
        }
        _ => payload_of(data.as_ref(), action),
    };

    if action != A11yAction::Click
        && app.a11y_has_handler(action)
        && run_handler(app, window, handler_node, action, payload)
    {
        return;
    }

    let handled = match action {
        A11yAction::Click => on_click(app, window, engine),
        A11yAction::Focus => on_focus(app, window, engine),
        A11yAction::ScrollUp
        | A11yAction::ScrollDown
        | A11yAction::ScrollLeft
        | A11yAction::ScrollRight => scroll_by(app, window, engine, action, data.as_ref()),
        A11yAction::ScrollIntoView => scroll_into_view(app, window, engine),
        A11yAction::SetScrollOffset => match payload {
            A11yActionData::ScrollOffset(offset) => scroll_to(app, window, engine, offset),
            _ => false,
        },
        A11yAction::SetValue
        | A11yAction::Increment
        | A11yAction::Decrement
        | A11yAction::SetTextSelection
        | A11yAction::Expand
        | A11yAction::Collapse
        | A11yAction::ShowContextMenu
        | A11yAction::Custom(_) => false,
    };
    if !handled {
        app.a11y_note(A11yDiagnostic::UnhandledAction {
            node: engine,
            action: Some(action),
        });
    }
}

const fn is_state_changing(action: A11yAction) -> bool {
    matches!(
        action,
        A11yAction::Click
            | A11yAction::Focus
            | A11yAction::SetValue
            | A11yAction::Increment
            | A11yAction::Decrement
            | A11yAction::SetTextSelection
            | A11yAction::Expand
            | A11yAction::Collapse
    )
}

fn increment_payload(
    app: &App,
    window: WindowId,
    node: crate::node::id::NodeId,
    action: A11yAction,
    data: Option<&ActionData>,
) -> A11yActionData<'static> {
    if let Some(ActionData::NumericValue(value)) = data {
        return A11yActionData::NumericValue(*value);
    }
    let Some(tree) = app.window(window).map(|target| target.tree()) else {
        return A11yActionData::None;
    };
    let slot = tree.a11y_of(node);
    let patch = slot.map(|slot| &slot.patch);
    let over = slot.and_then(|slot| slot.over.as_ref());
    let value = patch
        .and_then(|patch| patch.numeric_value)
        .or_else(|| over.and_then(|over| over.numeric_value));
    let step = patch
        .and_then(|patch| patch.numeric_value_step)
        .or_else(|| over.and_then(|over| over.numeric_value_step));
    match (value, step) {
        (Some(value), Some(step)) => {
            let delta = if action == A11yAction::Decrement {
                -step
            } else {
                step
            };
            A11yActionData::NumericValue(value + delta)
        }
        _ => A11yActionData::None,
    }
}

fn payload_of(data: Option<&ActionData>, action: A11yAction) -> A11yActionData<'_> {
    match (action, data) {
        (A11yAction::Custom(id), _) => A11yActionData::Custom(id),
        (_, Some(ActionData::Value(value))) => A11yActionData::Value(value),
        (_, Some(ActionData::NumericValue(value))) => A11yActionData::NumericValue(*value),
        (_, Some(ActionData::SetScrollOffset(point))) => {
            A11yActionData::ScrollOffset(LogicalPoint::new(px(point.x as f32), px(point.y as f32)))
        }
        _ => A11yActionData::None,
    }
}

fn selection_payload(
    app: &App,
    window: WindowId,
    target: NodeId,
    data: Option<&ActionData>,
) -> Option<(NodeId, A11yActionData<'static>)> {
    let Some(ActionData::SetTextSelection(selection)) = data else {
        return None;
    };
    let window_state = app.window(window)?;
    let tree = window_state.tree();
    let anchor = selection_position(
        tree,
        window_state,
        selection.anchor.node.0,
        selection.anchor.character_index,
        target,
    )?;
    let focus = selection_position(
        tree,
        window_state,
        selection.focus.node.0,
        selection.focus.character_index,
        target,
    )?;
    Some((
        normalized_selection_target(tree, target),
        A11yActionData::Selection { anchor, focus },
    ))
}

fn selection_position(
    tree: &NodeTree,
    window: &Window,
    stable: u64,
    character_index: usize,
    fallback: NodeId,
) -> Option<A11yTextPosition> {
    let run = window
        .a11y_engine_of(A11yNodeId::from_stable(stable))
        .filter(|engine| is_text_run(tree, *engine))
        .or_else(|| is_text_run(tree, fallback).then_some(fallback))?;
    let content = tree.text_content(run)?;
    let (lengths, _) = textrun::text_metrics(&content.text)?;
    Some(A11yTextPosition {
        node: run,
        byte_index: textrun::byte_index(&lengths, character_index),
    })
}

fn normalized_selection_target(tree: &NodeTree, target: NodeId) -> NodeId {
    if is_text_input(tree, target) {
        return target;
    }
    if !is_text_run(tree, target) {
        return target;
    }
    let mut cursor = tree.get(target).ok().and_then(Node::parent);
    let mut hops = 0;
    while let Some(parent) = cursor {
        if is_text_input(tree, parent) {
            return parent;
        }
        hops += 1;
        if hops >= 2 {
            break;
        }
        cursor = tree.get(parent).ok().and_then(Node::parent);
    }
    target
}

fn is_text_input(tree: &NodeTree, node: NodeId) -> bool {
    matches!(
        effective_role_of(tree, node),
        A11yRole::TextInput | A11yRole::MultilineTextInput
    )
}

fn is_text_run(tree: &NodeTree, node: NodeId) -> bool {
    tree.get(node)
        .is_ok_and(|target| matches!(target.kind(), NodeKind::Text(_)))
        && effective_role_of(tree, node) == A11yRole::TextRun
}

fn run_handler(
    app: &mut App,
    window: WindowId,
    node: crate::node::id::NodeId,
    action: A11yAction,
    data: A11yActionData<'_>,
) -> bool {
    let Some(handler) = app.a11y_take_handler(action) else {
        return false;
    };
    let mut guard = HandlerGuard {
        app,
        action,
        handler: Some(handler),
    };
    {
        let mut ctx = A11yActionCtx::new(guard.app, window, node, action, data);
        if let Some(handler) = guard.handler.as_mut() {
            handler(&mut ctx);
        }
    }
    true
}

struct HandlerGuard<'a> {
    app: &'a mut App,
    action: A11yAction,
    handler: Option<Box<dyn FnMut(&mut A11yActionCtx<'_>)>>,
}

impl Drop for HandlerGuard<'_> {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.app.a11y_put_handler(self.action, handler);
        }
    }
}

fn on_click(app: &mut App, window: WindowId, node: crate::node::id::NodeId) -> bool {
    let Some((target, action)) = app
        .window(window)
        .and_then(|window_state| action_target(window_state.tree(), node))
    else {
        return false;
    };
    app.dispatch_action(window, target, action, ActionSource::Programmatic)
}

fn on_focus(app: &mut App, window: WindowId, node: crate::node::id::NodeId) -> bool {
    let focusable_node = app
        .window(window)
        .is_some_and(|window_state| focusable(window_state.tree(), node));
    focusable_node && app.set_focus(window, Some(node))
}

fn scroll_container(
    app: &App,
    window: WindowId,
    node: crate::node::id::NodeId,
) -> Option<crate::node::id::NodeId> {
    let tree = app.window(window)?.tree();
    crate::a11y::tree::a11y_scroll_target(tree, node)
}

fn scroll_by(
    app: &mut App,
    window: WindowId,
    node: crate::node::id::NodeId,
    action: A11yAction,
    data: Option<&ActionData>,
) -> bool {
    let Some(container) = scroll_container(app, window, node) else {
        return false;
    };
    let Some(slot) = app
        .window(window)
        .and_then(|window_state| window_state.tree().scroll_slot(container))
    else {
        return false;
    };
    let viewport = crate::interaction::scroll::viewport(
        app.window(window).expect("窗口存在").tree(),
        container,
    );
    let step = match data {
        Some(ActionData::ScrollUnit(accesskit::ScrollUnit::Item)) => slot.line_height.max(px(1.0)),
        _ => viewport.size.height.max(slot.line_height).max(px(1.0)),
    };
    let delta = match action {
        A11yAction::ScrollUp => LogicalPoint::new(px(0.0), -step),
        A11yAction::ScrollDown => LogicalPoint::new(px(0.0), step),
        A11yAction::ScrollLeft => LogicalPoint::new(-step, px(0.0)),
        A11yAction::ScrollRight => LogicalPoint::new(step, px(0.0)),
        _ => LogicalPoint::ZERO,
    };
    apply_scroll(app, window, container, delta)
}

fn scroll_into_view(app: &mut App, window: WindowId, node: crate::node::id::NodeId) -> bool {
    let Some(container) = scroll_container(app, window, node) else {
        return false;
    };
    let Some(tree) = app.window(window).map(|window_state| window_state.tree()) else {
        return false;
    };
    let target = crate::interaction::scroll::to_screen(
        tree,
        node,
        tree.get(node)
            .map_or(crate::math::LogicalRect::default(), |state| {
                state.layout().bounds
            }),
    );
    let view = crate::interaction::scroll::viewport(tree, container);
    let mut delta = LogicalPoint::ZERO;
    if target.min().y < view.min().y {
        delta.y = target.min().y - view.min().y;
    } else if target.max().y > view.max().y {
        delta.y = target.max().y - view.max().y;
    }
    if target.min().x < view.min().x {
        delta.x = target.min().x - view.min().x;
    } else if target.max().x > view.max().x {
        delta.x = target.max().x - view.max().x;
    }
    apply_scroll(app, window, container, delta)
}

fn scroll_to(
    app: &mut App,
    window: WindowId,
    node: crate::node::id::NodeId,
    offset: LogicalPoint,
) -> bool {
    let Some(container) = scroll_container(app, window, node) else {
        return false;
    };
    let Some(handle) = app.scroll_handle(window, container) else {
        return false;
    };
    app.scroll_to(&handle, offset).is_ok()
}

fn apply_scroll(
    app: &mut App,
    window: WindowId,
    container: crate::node::id::NodeId,
    delta: LogicalPoint,
) -> bool {
    if delta.x == LogicalPx::ZERO && delta.y == LogicalPx::ZERO {
        return true;
    }
    let Some(handle) = app.scroll_handle(window, container) else {
        return false;
    };
    app.scroll_by(&handle, delta).is_ok()
}

fn note_stale(app: &mut App, action: Action) {
    app.a11y_note(A11yDiagnostic::StaleAction {
        action: action_from_accesskit(action),
    });
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use accesskit::{Action as AkAction, ActionData, ActionRequest, NodeId as AkNodeId, TreeId};
    use smol_str::SmolStr;

    use super::dispatch;
    use crate::{
        a11y::{A11yAction, A11yActionData, A11yOverride, A11yPatch},
        app::App,
        event::Action,
        interaction::ScrollConfig,
        math::{LogicalPoint, LogicalRect, px},
        node::builder::NodeBuilder,
        paint::style::StylePatch,
        window::WindowOptions,
    };

    fn setup() -> (tokio::runtime::Runtime, App, crate::window::WindowId) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions::default());
        (runtime, app, window)
    }

    fn request(
        app: &mut App,
        window: crate::window::WindowId,
        action: AkAction,
        target: crate::node::id::NodeId,
        data: Option<ActionData>,
    ) -> ActionRequest {
        let stable = app
            .window(window)
            .and_then(|state| state.a11y_stable_id(target))
            .or_else(|| {
                let _ = app.a11y_take(window);
                app.window(window)
                    .and_then(|state| state.a11y_stable_id(target))
            })
            .expect("a11y 分配器必须给目标节点发过 id");
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(stable.get()),
            data,
        }
    }

    fn dispatch_request(
        app: &mut App,
        window: crate::window::WindowId,
        action: AkAction,
        target: crate::node::id::NodeId,
        data: Option<ActionData>,
    ) {
        let request = request(app, window, action, target, data);
        dispatch(app, window, request);
    }

    fn assert_no_action_diagnostics(app: &App, message: &str) {
        let offender = app.a11y_diagnostics().iter().find(|diagnostic| {
            matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::UnhandledAction { .. }
                    | crate::a11y::A11yDiagnostic::DisabledAction { .. }
                    | crate::a11y::A11yDiagnostic::ReadOnlyAction { .. }
                    | crate::a11y::A11yDiagnostic::UnadvertisedAction { .. }
                    | crate::a11y::A11yDiagnostic::StaleAction { .. }
            )
        });
        assert!(offender.is_none(), "{message}: {offender:?}");
    }

    #[test]
    fn numeric_set_value_reaches_the_handler() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::Slider),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let seen = Rc::new(Cell::new(f64::NAN));
        let slot = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::SetValue, move |ctx| {
            if let A11yActionData::NumericValue(value) = ctx.data() {
                slot.set(value);
            }
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::SetValue,
            node,
            Some(ActionData::NumericValue(42.5)),
        );

        assert_eq!(seen.get(), 42.5, "handler 必须收到 UIA 传的数值");
        assert_no_action_diagnostics(&app, "有 handler 时不得记未处理诊断");
    }

    #[test]
    fn increment_and_decrement_are_routed_to_the_handler() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::Slider),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let count = Rc::new(Cell::new(0_u32));
        let counter = Rc::clone(&count);
        app.on_a11y_action(A11yAction::Increment, move |_ctx| {
            counter.set(counter.get() + 1);
        });
        dispatch_request(&mut app, window, AkAction::Increment, node, None);
        assert_eq!(count.get(), 1, "Increment 必须落到 handler");
    }

    #[test]
    fn click_programmatically_dispatches_the_declared_action() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0)))
            .on_click(Action::custom("press"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let hits = Rc::new(Cell::new(0));
        let counter = Rc::clone(&hits);
        app.on_action(Action::custom("press"), move |_ctx| {
            counter.set(counter.get() + 1);
        });

        dispatch_request(&mut app, window, AkAction::Click, node, None);

        assert_eq!(hits.get(), 1, "Click 必须落到 Action 表");
        assert_no_action_diagnostics(&app, "Click 落地后不得记未处理诊断");
    }

    #[test]
    fn focus_and_blur_move_the_focus_scope() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .on_click(Action::custom("press"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");

        dispatch_request(&mut app, window, AkAction::Focus, node, None);
        assert_eq!(app.focused(window), Some(node));

        dispatch_request(&mut app, window, AkAction::Blur, node, None);
        assert_eq!(app.focused(window), None);
    }

    #[test]
    fn a11y_focus_wakes_the_event_loop() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .on_click(Action::custom("press"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let _ = app.dispatcher().take_frame_wake();

        dispatch_request(&mut app, window, AkAction::Focus, node, None);

        assert_eq!(app.focused(window), Some(node));
        assert!(
            app.dispatcher().take_frame_wake(),
            "a11y Focus 必须唤醒事件循环（回投任务里改状态）"
        );
    }

    #[test]
    fn a11y_actions_outside_the_modal_scope_are_rejected() {
        use crate::interaction::{OverlayDismiss, OverlayOptions};

        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let background = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::Slider),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let mut style = crate::paint::style_builder::StyleBuilder::new()
            .relative()
            .size(px(100.0), px(80.0))
            .build();
        style.clip = crate::paint::style::Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("container");
        let handle = app
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.set_scroll_content(&handle, crate::math::LogicalSize::new(px(100.0), px(400.0)))
            .expect("content");

        let set_value_hits = Rc::new(Cell::new(0_u32));
        let seen_value = Rc::clone(&set_value_hits);
        app.on_a11y_action(A11yAction::SetValue, move |_ctx| {
            seen_value.set(seen_value.get() + 1);
        });
        let increment_hits = Rc::new(Cell::new(0_u32));
        let seen_increment = Rc::clone(&increment_hits);
        app.on_a11y_action(A11yAction::Increment, move |_ctx| {
            seen_increment.set(seen_increment.get() + 1);
        });

        let modal = app
            .create_overlay(
                window,
                OverlayOptions {
                    trap_focus: true,
                    dismiss: OverlayDismiss::Explicit,
                    ..OverlayOptions::default()
                },
            )
            .expect("modal");
        let inside = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::Slider),
                ..A11yOverride::default()
            })
            .build(
                app.window_mut(window).expect("window").tree_mut(),
                modal.node(),
            )
            .expect("mount");

        dispatch_request(
            &mut app,
            window,
            AkAction::SetValue,
            background,
            Some(ActionData::Value("9".into())),
        );
        dispatch_request(&mut app, window, AkAction::Increment, background, None);
        dispatch_request(
            &mut app,
            window,
            AkAction::ScrollDown,
            container,
            Some(ActionData::ScrollUnit(accesskit::ScrollUnit::Item)),
        );
        assert_eq!(set_value_hits.get(), 0, "模态外的 SetValue 不得生效");
        assert_eq!(increment_hits.get(), 0, "模态外的 Increment 不得生效");
        assert_eq!(
            app.scroll_state(&handle).expect("state").offset.y,
            px(0.0),
            "模态外的容器不得被 AT 滚动"
        );
        assert!(
            app.a11y_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::UnhandledAction { .. }
            )),
            "被作用域拒绝的动作必须记诊断：{:?}",
            app.a11y_diagnostics()
        );

        dispatch_request(
            &mut app,
            window,
            AkAction::SetValue,
            inside,
            Some(ActionData::Value("9".into())),
        );
        dispatch_request(&mut app, window, AkAction::Increment, inside, None);
        assert_eq!(set_value_hits.get(), 1);
        assert_eq!(increment_hits.get(), 1);
    }

    #[test]
    fn stale_target_is_ignored_and_diagnosed() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let doomed = NodeBuilder::boxed()
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let _ = app.a11y_take(window).expect("首笔交付");
        let stable = app
            .window(window)
            .expect("window")
            .a11y_stable_id(doomed)
            .expect("doomed 的稳定 id");
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .remove(doomed)
            .expect("remove");

        let hits = Rc::new(Cell::new(0));
        let counter = Rc::clone(&hits);
        app.on_action(Action::custom("press"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
        dispatch(
            &mut app,
            window,
            ActionRequest {
                action: AkAction::Click,
                target_tree: TreeId::ROOT,
                target_node: AkNodeId(stable.get()),
                data: None,
            },
        );

        assert_eq!(hits.get(), 0, "陈旧目标不得触发任何 handler");
        assert!(matches!(
            app.a11y_diagnostics(),
            [crate::a11y::A11yDiagnostic::StaleAction { .. }]
        ));
    }

    #[test]
    fn app_handler_receives_the_payload() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::TextInput),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let seen = Rc::new(Cell::new(String::new()));
        let sink = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::SetValue, move |ctx| {
            if let A11yActionData::Value(text) = ctx.data() {
                sink.set(String::from(text));
            }
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::SetValue,
            node,
            Some(ActionData::Value("42".into())),
        );
        assert_eq!(seen.take(), String::from("42"));
        assert_no_action_diagnostics(&app, "handler 落地后不再记 Unhandled");
        let _ = SmolStr::default();
    }

    #[test]
    fn replace_selected_text_is_normalised_to_set_value() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::TextInput),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let seen = Rc::new(Cell::new(String::new()));
        let sink = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::SetValue, move |ctx| {
            if let A11yActionData::Value(text) = ctx.data() {
                sink.set(String::from(text));
            }
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::ReplaceSelectedText,
            node,
            Some(ActionData::Value("的".into())),
        );
        assert_eq!(seen.take(), String::from("的"));
    }

    fn text_input_fixture(
        app: &mut App,
        window: crate::window::WindowId,
    ) -> (
        crate::node::id::NodeId,
        crate::node::id::NodeId,
        crate::a11y::A11yNodeId,
    ) {
        let root = app.window(window).expect("window").tree().root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::TextInput),
                label: Some(SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("input");
        let text = NodeBuilder::text("价🌍ab", crate::text::TextStyle::new(px(12.0)))
            .build(app.window_mut(window).expect("window").tree_mut(), input)
            .expect("text run");
        let _ = app
            .a11y_take(window)
            .expect("首笔 a11y 交付（分配稳定 id）");
        let stable = app
            .window(window)
            .expect("window")
            .a11y_stable_id(text)
            .expect("text run 的稳定 id");
        (input, text, stable)
    }

    #[test]
    fn set_text_selection_uses_byte_offsets_in_the_host_run() {
        let (_runtime, mut app, window) = setup();
        let (input, text, run) = text_input_fixture(&mut app, window);
        let seen = Rc::new(Cell::new(
            None::<(
                crate::a11y::A11yTextPosition,
                crate::a11y::A11yTextPosition,
                crate::node::id::NodeId,
            )>,
        ));
        let sink = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::SetTextSelection, move |ctx| {
            if let A11yActionData::Selection { anchor, focus } = ctx.data() {
                sink.set(Some((anchor, focus, ctx.node())));
            }
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::SetTextSelection,
            input,
            Some(ActionData::SetTextSelection(accesskit::TextSelection {
                anchor: accesskit::TextPosition {
                    node: AkNodeId(run.get()),
                    character_index: 1,
                },
                focus: accesskit::TextPosition {
                    node: AkNodeId(run.get()),
                    character_index: 3,
                },
            })),
        );
        assert_eq!(
            seen.get(),
            Some((
                crate::a11y::A11yTextPosition {
                    node: text,
                    byte_index: 3,
                },
                crate::a11y::A11yTextPosition {
                    node: text,
                    byte_index: 8,
                },
                input,
            )),
            "grapheme 1..3 → 字节 3..8（CJK 3 + emoji 4）；两端宿主 = 该 TextRun；目标归一为输入框"
        );
        assert_no_action_diagnostics(&app, "合法选区不得记未处理诊断");
    }

    #[test]
    fn set_text_selection_normalizes_the_target_to_the_input() {
        let (_runtime, mut app, window) = setup();
        let (input, text, run) = text_input_fixture(&mut app, window);
        let seen = Rc::new(Cell::new(None::<crate::node::id::NodeId>));
        let sink = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::SetTextSelection, move |ctx| {
            if matches!(ctx.data(), A11yActionData::Selection { .. }) {
                sink.set(Some(ctx.node()));
            }
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::SetTextSelection,
            text,
            Some(ActionData::SetTextSelection(accesskit::TextSelection {
                anchor: accesskit::TextPosition {
                    node: AkNodeId(run.get()),
                    character_index: 0,
                },
                focus: accesskit::TextPosition {
                    node: AkNodeId(run.get()),
                    character_index: 2,
                },
            })),
        );
        assert_eq!(
            seen.get(),
            Some(input),
            "AT 打 TextRun 时 handler 仍收到输入框（归一化）"
        );
    }

    #[test]
    fn an_unresolvable_text_selection_is_rejected() {
        let (_runtime, mut app, window) = setup();
        let (input, _text, _run) = text_input_fixture(&mut app, window);
        let hits = Rc::new(Cell::new(0_u32));
        let counter = Rc::clone(&hits);
        app.on_a11y_action(A11yAction::SetTextSelection, move |_ctx| {
            counter.set(counter.get() + 1);
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::SetTextSelection,
            input,
            Some(ActionData::SetTextSelection(accesskit::TextSelection {
                anchor: accesskit::TextPosition {
                    node: AkNodeId(0),
                    character_index: 1,
                },
                focus: accesskit::TextPosition {
                    node: AkNodeId(0),
                    character_index: 3,
                },
            })),
        );
        assert_eq!(
            hits.get(),
            0,
            "无法解析的选区 payload 不得落到 handler（口径不可信）"
        );
        assert!(
            app.a11y_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::InvalidActionData {
                    action: A11yAction::SetTextSelection,
                    ..
                }
            )),
            "拒绝必须留 InvalidActionData 诊断：{:?}",
            app.a11y_diagnostics()
        );
    }

    fn two_run_text_input_fixture(
        app: &mut App,
        window: crate::window::WindowId,
    ) -> (
        crate::node::id::NodeId,
        crate::node::id::NodeId,
        crate::node::id::NodeId,
        crate::a11y::A11yNodeId,
        crate::a11y::A11yNodeId,
    ) {
        let root = app.window(window).expect("window").tree().root();
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::TextInput),
                label: Some(SmolStr::new_static("价格")),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("input");
        let first = NodeBuilder::text("价格", crate::text::TextStyle::new(px(12.0)))
            .build(app.window_mut(window).expect("window").tree_mut(), input)
            .expect("first run");
        let second = NodeBuilder::text("42", crate::text::TextStyle::new(px(12.0)))
            .build(app.window_mut(window).expect("window").tree_mut(), input)
            .expect("second run");
        let _ = app
            .a11y_take(window)
            .expect("首笔 a11y 交付（分配稳定 id）");
        let state = app.window(window).expect("window");
        let first_stable = state.a11y_stable_id(first).expect("first run 的稳定 id");
        let second_stable = state.a11y_stable_id(second).expect("second run 的稳定 id");
        (input, first, second, first_stable, second_stable)
    }

    #[test]
    fn a_selection_across_two_text_runs_keeps_each_end_its_run_identity() {
        let (_runtime, mut app, window) = setup();
        let (input, first, second, first_stable, second_stable) =
            two_run_text_input_fixture(&mut app, window);
        let seen = Rc::new(Cell::new(
            None::<(
                crate::a11y::A11yTextPosition,
                crate::a11y::A11yTextPosition,
                crate::node::id::NodeId,
            )>,
        ));
        let sink = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::SetTextSelection, move |ctx| {
            if let A11yActionData::Selection { anchor, focus } = ctx.data() {
                sink.set(Some((anchor, focus, ctx.node())));
            }
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::SetTextSelection,
            input,
            Some(ActionData::SetTextSelection(accesskit::TextSelection {
                anchor: accesskit::TextPosition {
                    node: AkNodeId(first_stable.get()),
                    character_index: 2,
                },
                focus: accesskit::TextPosition {
                    node: AkNodeId(second_stable.get()),
                    character_index: 1,
                },
            })),
        );
        assert_eq!(
            seen.get(),
            Some((
                crate::a11y::A11yTextPosition {
                    node: first,
                    byte_index: 6,
                },
                crate::a11y::A11yTextPosition {
                    node: second,
                    byte_index: 1,
                },
                input,
            )),
            "跨 run 选区：anchor 在 run「价格」末尾（字节 6）、focus 在 run「42」的第 1 字节，\
             两端身份各自独立；ctx.node() 仍是输入框"
        );
        assert_no_action_diagnostics(&app, "合法跨 run 选区不得记未处理诊断");
    }

    #[test]
    fn an_out_of_range_character_index_clamps_inside_the_host_run() {
        let (_runtime, mut app, window) = setup();
        let (input, first, second, first_stable, second_stable) =
            two_run_text_input_fixture(&mut app, window);
        let seen = Rc::new(Cell::new(
            None::<(crate::a11y::A11yTextPosition, crate::a11y::A11yTextPosition)>,
        ));
        let sink = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::SetTextSelection, move |ctx| {
            if let A11yActionData::Selection { anchor, focus } = ctx.data() {
                sink.set(Some((anchor, focus)));
            }
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::SetTextSelection,
            input,
            Some(ActionData::SetTextSelection(accesskit::TextSelection {
                anchor: accesskit::TextPosition {
                    node: AkNodeId(first_stable.get()),
                    character_index: 99,
                },
                focus: accesskit::TextPosition {
                    node: AkNodeId(second_stable.get()),
                    character_index: 99,
                },
            })),
        );
        assert_eq!(
            seen.get(),
            Some((
                crate::a11y::A11yTextPosition {
                    node: first,
                    byte_index: 6,
                },
                crate::a11y::A11yTextPosition {
                    node: second,
                    byte_index: 2,
                },
            )),
            "越界下标夹紧到各自 run 的文本末尾（不是另一端的 run）"
        );
        assert_no_action_diagnostics(&app, "夹紧仍属合法 payload");
    }

    #[test]
    fn scroll_unit_moves_the_container() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let mut style = crate::paint::style_builder::StyleBuilder::new()
            .relative()
            .size(px(100.0), px(80.0))
            .build();
        style.clip = crate::paint::style::Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("container");
        let handle = app
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.set_scroll_content(&handle, crate::math::LogicalSize::new(px(100.0), px(400.0)))
            .expect("content");

        dispatch_request(
            &mut app,
            window,
            AkAction::ScrollDown,
            container,
            Some(ActionData::ScrollUnit(accesskit::ScrollUnit::Item)),
        );
        let state = app.scroll_state(&handle).expect("state");
        assert!(state.offset.y > px(0.0), "ScrollDown 必须下移偏移");
        assert!(app.a11y_diagnostics().is_empty(), "滚动必须被处理");
    }

    #[test]
    fn unmapped_action_is_diagnosed() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        dispatch_request(&mut app, window, AkAction::ShowTooltip, node, None);
        assert!(matches!(
            app.a11y_diagnostics(),
            [crate::a11y::A11yDiagnostic::UnhandledAction { .. }]
        ));
        let _ = (
            A11yPatch::default(),
            StylePatch::default(),
            ScrollConfig::default(),
            LogicalPoint::ZERO,
        );
    }

    #[test]
    fn disabled_ancestors_block_actions_on_descendants() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let tree = app.window_mut(window).expect("window").tree_mut();
        let panel = NodeBuilder::boxed().build(tree, root).expect("panel");
        let child = NodeBuilder::boxed()
            .on_click(Action::custom("buy"))
            .build(tree, panel)
            .expect("child");
        tree.set_style_patch_for_test(
            panel,
            StylePatch {
                interactive: Some(false),
                ..StylePatch::default()
            },
        );
        let hits = Rc::new(Cell::new(0));
        let counter = Rc::clone(&hits);
        app.on_action(Action::custom("buy"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
        dispatch_request(&mut app, window, AkAction::Click, child, None);
        assert_eq!(hits.get(), 0, "禁用祖先的后代不得被 AT 激活");
        assert!(
            app.a11y_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::DisabledAction { .. }
            )),
            "{:?}",
            app.a11y_diagnostics()
        );
    }

    #[test]
    fn unknown_stable_ids_are_stale() {
        let (_runtime, mut app, window) = setup();
        dispatch(
            &mut app,
            window,
            ActionRequest {
                action: AkAction::Click,
                target_tree: TreeId::ROOT,
                target_node: AkNodeId(999_999),
                data: None,
            },
        );
        assert!(matches!(
            app.a11y_diagnostics(),
            [crate::a11y::A11yDiagnostic::StaleAction { .. }]
        ));
    }

    #[test]
    fn disabled_nodes_reject_state_changing_actions() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .on_click(Action::custom("buy"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let hits = Rc::new(Cell::new(0));
        let counter = Rc::clone(&hits);
        app.on_action(Action::custom("buy"), move |_ctx| {
            counter.set(counter.get() + 1);
        });
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_a11y_patch(
                node,
                A11yPatch {
                    disabled: Some(true),
                    ..A11yPatch::default()
                },
            );
        dispatch_request(&mut app, window, AkAction::Click, node, None);
        assert_eq!(hits.get(), 0, "禁用节点不得被 AT 激活（可能下单）");
        assert!(
            app.a11y_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::DisabledAction { .. }
            )),
            "{:?}",
            app.a11y_diagnostics()
        );
    }

    #[test]
    fn increment_payload_uses_the_numeric_value_and_step() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::Slider),
                label: Some(SmolStr::new_static("价格")),
                numeric_value: Some(100.0),
                numeric_value_step: Some(5.0),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let seen = Rc::new(Cell::new(f64::NAN));
        let slot = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::Increment, move |ctx| {
            if let A11yActionData::NumericValue(value) = ctx.data() {
                slot.set(value);
            }
        });
        let seen_down = Rc::new(Cell::new(f64::NAN));
        let slot_down = Rc::clone(&seen_down);
        app.on_a11y_action(A11yAction::Decrement, move |ctx| {
            if let A11yActionData::NumericValue(value) = ctx.data() {
                slot_down.set(value);
            }
        });
        dispatch_request(&mut app, window, AkAction::Increment, node, None);
        assert_eq!(seen.get(), 105.0, "Increment = 当前值 + 步长");
        dispatch_request(&mut app, window, AkAction::Decrement, node, None);
        assert_eq!(seen_down.get(), 95.0, "Decrement = 当前值 - 步长");
    }

    #[test]
    fn a_panicking_handler_is_restored() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                actions: Some(vec![A11yAction::SetValue]),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let calls = Rc::new(Cell::new(0));
        let counter = Rc::clone(&calls);
        app.on_a11y_action(A11yAction::SetValue, move |_ctx| {
            counter.set(counter.get() + 1);
            panic!("应用 handler 炸了");
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_request(&mut app, window, AkAction::SetValue, node, None);
        }));
        assert!(outcome.is_err(), "panic 必须冒泡给上层（引擎不吞）");
        assert!(
            app.a11y_has_handler(A11yAction::SetValue),
            "panic 后 handler 必须被放回（否则后续同类动作永久失效）"
        );
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_request(&mut app, window, AkAction::SetValue, node, None);
        }));
        assert!(outcome.is_err());
        assert_eq!(calls.get(), 2, "第二次分发仍然命中 handler");
    }

    #[test]
    fn read_only_text_input_does_not_advertise_or_execute_set_value() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .on_click(Action::custom("focus"))
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::TextInput),
                label: Some(SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_a11y_patch(
                node,
                A11yPatch {
                    read_only: Some(true),
                    ..A11yPatch::default()
                },
            );

        let snapshot = app.a11y_take(window).expect("首笔 a11y 交付");
        let record = snapshot.node_for(node).expect("input");
        assert!(record.read_only, "节点必须宣告 read_only");
        assert!(!record.disabled, "read_only 不是 disabled");
        assert!(
            !record.actions.contains(&A11yAction::SetValue),
            "只读节点不得宣告 SetValue：{:?}",
            record.actions
        );
        assert!(
            record.actions.contains(&A11yAction::Focus),
            "read_only 只剔除值变更能力，Focus 必须保留：{:?}",
            record.actions
        );
        let ak = crate::a11y::adapter::to_accesskit(record);
        assert!(ak.is_read_only(), "AccessKit 侧必须带 IsReadOnly");
        assert!(
            !ak.supports_action(accesskit::Action::SetValue),
            "只读节点不得向 AT 宣告 SetValue"
        );

        let hits = Rc::new(Cell::new(0_u32));
        let counter = Rc::clone(&hits);
        app.on_a11y_action(A11yAction::SetValue, move |_ctx| {
            counter.set(counter.get() + 1);
        });
        dispatch_request(
            &mut app,
            window,
            AkAction::SetValue,
            node,
            Some(ActionData::Value("x".into())),
        );
        assert_eq!(hits.get(), 0, "只读节点的 SetValue 不得落到应用 handler");
        assert!(
            app.a11y_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::ReadOnlyAction {
                    action: A11yAction::SetValue,
                    ..
                }
            )),
            "拒绝只读节点的值变更必须留诊断：{:?}",
            app.a11y_diagnostics()
        );
    }

    #[test]
    fn read_only_keeps_blur_custom_and_selection() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let custom = crate::a11y::A11yCustomAction {
            id: 7,
            label: SmolStr::new_static("加入自选"),
        };
        let node = NodeBuilder::boxed()
            .on_click(Action::custom("focus"))
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::TextInput),
                label: Some(SmolStr::new_static("搜索")),
                actions: Some(vec![
                    A11yAction::SetValue,
                    A11yAction::SetTextSelection,
                    A11yAction::Custom(7),
                ]),
                custom_actions: vec![custom],
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .set_a11y_patch(
                node,
                A11yPatch {
                    read_only: Some(true),
                    ..A11yPatch::default()
                },
            );
        let text = NodeBuilder::text("搜索框", crate::text::TextStyle::new(px(12.0)))
            .build(app.window_mut(window).expect("window").tree_mut(), node)
            .expect("text run");
        let _ = app
            .a11y_take(window)
            .expect("首笔 a11y 交付（分配稳定 id）");
        let run = app
            .window(window)
            .expect("window")
            .a11y_stable_id(text)
            .expect("text run 的稳定 id");

        let custom_hits = Rc::new(Cell::new(0_u32));
        let seen_custom = Rc::clone(&custom_hits);
        app.on_a11y_action(A11yAction::Custom(7), move |_ctx| {
            seen_custom.set(seen_custom.get() + 1);
        });
        let selection_hits = Rc::new(Cell::new(0_u32));
        let seen_selection = Rc::clone(&selection_hits);
        let selection_target = Rc::new(Cell::new(None::<crate::node::id::NodeId>));
        let seen_target = Rc::clone(&selection_target);
        app.on_a11y_action(A11yAction::SetTextSelection, move |ctx| {
            if matches!(ctx.data(), A11yActionData::Selection { .. }) {
                seen_selection.set(seen_selection.get() + 1);
                seen_target.set(Some(ctx.node()));
            }
        });

        dispatch_request(
            &mut app,
            window,
            AkAction::CustomAction,
            node,
            Some(ActionData::CustomAction(7)),
        );
        dispatch_request(
            &mut app,
            window,
            AkAction::SetTextSelection,
            node,
            Some(ActionData::SetTextSelection(accesskit::TextSelection {
                anchor: accesskit::TextPosition {
                    node: AkNodeId(run.get()),
                    character_index: 1,
                },
                focus: accesskit::TextPosition {
                    node: AkNodeId(run.get()),
                    character_index: 3,
                },
            })),
        );
        assert_eq!(custom_hits.get(), 1, "read_only 不影响自定义动作");
        assert_eq!(
            selection_hits.get(),
            1,
            "read_only 不影响选区（只读文本仍可选中朗读）"
        );
        assert_eq!(
            selection_target.get(),
            Some(node),
            "选区动作的 handler 目标归一为输入框"
        );

        dispatch_request(&mut app, window, AkAction::Focus, node, None);
        assert_eq!(app.focused(window), Some(node));
        dispatch_request(&mut app, window, AkAction::Blur, node, None);
        assert_eq!(app.focused(window), None, "read_only 不阻断 Blur");
    }

    #[test]
    fn scroll_actions_converge_to_the_scroll_container() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let mut style = crate::paint::style_builder::StyleBuilder::new()
            .relative()
            .size(px(100.0), px(80.0))
            .build();
        style.clip = crate::paint::style::Clip::Bounds;
        let container = NodeBuilder::boxed()
            .style(style)
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("container");
        let row = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::ListBoxOption),
                label: Some(SmolStr::new_static("行")),
                ..A11yOverride::default()
            })
            .build(
                app.window_mut(window).expect("window").tree_mut(),
                container,
            )
            .expect("row");
        let handle = app
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.set_scroll_content(&handle, crate::math::LogicalSize::new(px(100.0), px(400.0)))
            .expect("content");

        dispatch_request(
            &mut app,
            window,
            AkAction::ScrollDown,
            row,
            Some(ActionData::ScrollUnit(accesskit::ScrollUnit::Item)),
        );
        assert!(
            app.scroll_state(&handle).expect("state").offset.y > px(0.0),
            "AT 对行发 ScrollDown 必须滚动其容器"
        );
        assert_no_action_diagnostics(&app, "滚动容器收敛后不得记能力拒绝");
    }

    #[test]
    fn undeclared_actions_are_not_dispatched_to_handlers() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let plain = NodeBuilder::boxed()
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("plain");
        let hits = Rc::new(Cell::new(0_u32));
        let counter = Rc::clone(&hits);
        app.on_a11y_action(A11yAction::SetValue, move |_ctx| {
            counter.set(counter.get() + 1);
        });
        dispatch_request(
            &mut app,
            window,
            AkAction::SetValue,
            plain,
            Some(ActionData::Value("注入".into())),
        );
        assert_eq!(
            hits.get(),
            0,
            "未声明 SetValue 的节点不得把动作交给 handler"
        );
        assert!(
            app.a11y_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                crate::a11y::A11yDiagnostic::UnadvertisedAction {
                    action: A11yAction::SetValue,
                    ..
                }
            )),
            "能力拒绝必须留诊断：{:?}",
            app.a11y_diagnostics()
        );
    }

    #[test]
    fn advertised_actions_are_the_dispatch_trust_boundary() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let button = NodeBuilder::boxed()
            .on_click(Action::custom("press"))
            .a11y(A11yOverride {
                label: Some(SmolStr::new_static("下单")),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("button");
        let slider = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::Slider),
                label: Some(SmolStr::new_static("价格")),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("slider");
        let input = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::TextInput),
                label: Some(SmolStr::new_static("搜索")),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("input");
        let plain = NodeBuilder::boxed()
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("plain");
        let snapshot = app.a11y_take(window).expect("首笔 a11y 交付");

        const ALL: [A11yAction; 15] = [
            A11yAction::Click,
            A11yAction::Focus,
            A11yAction::SetValue,
            A11yAction::Increment,
            A11yAction::Decrement,
            A11yAction::SetTextSelection,
            A11yAction::ScrollUp,
            A11yAction::ScrollDown,
            A11yAction::ScrollLeft,
            A11yAction::ScrollRight,
            A11yAction::ScrollIntoView,
            A11yAction::SetScrollOffset,
            A11yAction::Expand,
            A11yAction::Collapse,
            A11yAction::ShowContextMenu,
        ];
        let tree = app.window(window).expect("window").tree();
        for node in [button, slider, input, plain] {
            let record = snapshot.node_for(node).expect("node record");
            for action in ALL {
                if record.actions.contains(&action) {
                    assert!(
                        crate::a11y::mapping::action_allowed(tree, node, action),
                        "快照宣告的动作必须被分发门禁允许：{action:?} on {record:?}"
                    );
                }
            }
        }
        assert!(!crate::a11y::mapping::action_allowed(
            tree,
            plain,
            A11yAction::SetValue
        ));
        assert!(!crate::a11y::mapping::action_allowed(
            tree,
            plain,
            A11yAction::ScrollDown
        ));
        assert!(!crate::a11y::mapping::action_allowed(
            tree,
            plain,
            A11yAction::Custom(99)
        ));
    }
}
