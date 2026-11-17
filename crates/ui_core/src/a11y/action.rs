//! `ActionRequest` → 引擎动作（`design/phase-11-a11y.md` §9）。
//!
//! 执行点**只有主线程**：平台层的 `ActionHandler` 只把请求 `Dispatcher::push` 回主线程，本文件在
//! drain 时被调用。所有效果都走 P03/P10 已存在的失效闭环（`set_focus` / `dispatch_action` /
//! `scroll_*` / `App::write`），**不引入第二条写路径**。
//!
//! **模态作用域**（AR1b N2）：本文件的 [`dispatch`] 是 a11y 动作的**唯一入口**，入口处统一做
//! 一次作用域校验（与指针、键盘同一判据 [`crate::interaction::focus::in_scope`]）。`Click` /
//! `Focus` 之后还会各自经过 `dispatch_action` / `set_focus` 的校验，但 `SetValue` /
//! `Increment` / `Custom` / `Scroll*` 直接调应用闭包与 `scroll_*`，**不经过**那两个入口 ——
//! 漏掉入口校验的话，辅助技术在模态打开时仍能改背景控件状态、滚背景容器。

use accesskit::{Action, ActionData, ActionRequest};

use crate::{
    a11y::{
        A11yAction, A11yActionCtx, A11yActionData, A11yDiagnostic, A11yNodeId,
        mapping::action_from_accesskit,
    },
    app::App,
    event::ActionSource,
    interaction::focus::{action_target, focusable, in_scope},
    math::Point,
    window::WindowId,
};

/// 分发一条 a11y 动作（主线程）。
///
/// 陈旧 `NodeId` / 未映射动作 / 无 handler / **模态作用域外的目标**一律忽略 + 诊断，不 panic。
pub(crate) fn dispatch(app: &mut App, window: WindowId, request: ActionRequest) {
    if request.target_tree != accesskit::TreeId::ROOT {
        return;
    }
    let Some(engine) = A11yNodeId::from_bits(request.target_node.0).to_engine() else {
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

    // 模态作用域（AR1b N2）：与指针按下 / 滚轮 / Enter 同一条判据（`in_scope` = 模态子树
    // ∪ 属于该模态的浮层链）。`Click` 与 `Focus` 在下面还会各自经过 `dispatch_action` /
    // `set_focus` 的校验，但本入口是**唯一**能让 `SetValue` / `Increment` / `Custom` /
    // `Scroll*` 落地的地方，所以校验必须在这里做一次。
    let scope = app.focus_scope(window);
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

    // payload 的所有权先取出：`A11yActionCtx` 借用它。
    let data = request.data;
    // `Blur` 没有对应的 `A11yAction`（它不是「对某个节点做什么」，而是「把焦点从这个节点拿走」）。
    if request.action == Action::Blur {
        if app.focused(window) == Some(engine) {
            app.set_focus(window, None);
        }
        return;
    }
    let action = match (request.action, data.as_ref()) {
        (Action::CustomAction, Some(ActionData::CustomAction(id))) => A11yAction::Custom(*id),
        // 没有选区模型时 `ReplaceSelectedText` 与 `SetValue` 等价（design §9.1）。
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
    let payload = payload_of(data.as_ref(), action);

    // ① 应用 handler 优先（`Click` 例外：它必须落到 P10 的 `Action` 表，见 §9.3）。
    if action != A11yAction::Click
        && app.a11y_has_handler(action)
        && run_handler(app, window, engine, action, payload)
    {
        return;
    }

    // ② 内置映射。
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
        // 引擎没有文本模型 / 展开语义 / 菜单模型 / 数值模型：只有应用 handler 能落地。
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

/// 把 `accesskit::ActionData` 翻译成引擎 payload。
fn payload_of(data: Option<&ActionData>, action: A11yAction) -> A11yActionData<'_> {
    match (action, data) {
        (A11yAction::Custom(id), _) => A11yActionData::Custom(id),
        (_, Some(ActionData::Value(value))) => A11yActionData::Value(value),
        // UIA 的 `RangeValuePattern.SetValue` 发的是 `NumericValue(f64)` —— 不是文本值。
        // 旧实现把它落到 `A11yActionData::None`，应用无法知道 AT 要求的目标数值，
        // 于是「滑块能被 AT 读到数值、却不能被 AT 设值」（review P2-5）。
        (_, Some(ActionData::NumericValue(value))) => A11yActionData::NumericValue(*value),
        (_, Some(ActionData::SetTextSelection(selection))) => A11yActionData::Selection {
            anchor: selection.anchor.character_index,
            focus: selection.focus.character_index,
        },
        (_, Some(ActionData::SetScrollOffset(point))) => {
            A11yActionData::ScrollOffset(Point::new(point.x as f32, point.y as f32))
        }
        _ => A11yActionData::None,
    }
}

/// 调应用 handler（借用纪律：先摘出闭包 → 建 ctx → 调用 → 放回）。
fn run_handler(
    app: &mut App,
    window: WindowId,
    node: crate::node::id::NodeId,
    action: A11yAction,
    data: A11yActionData<'_>,
) -> bool {
    let Some(mut handler) = app.a11y_take_handler(action) else {
        return false;
    };
    {
        let mut ctx = A11yActionCtx::new(app, window, node, action, data);
        handler(&mut ctx);
    }
    app.a11y_put_handler(action, handler);
    true
}

/// `Click`：激活节点**声明的**动作（与指针点击 / 键盘 Enter 同一身份）。
fn on_click(app: &mut App, window: WindowId, node: crate::node::id::NodeId) -> bool {
    let Some((target, action)) = app
        .window(window)
        .and_then(|window_state| action_target(window_state.tree(), node))
    else {
        return false;
    };
    app.dispatch_action(window, target, action, ActionSource::Programmatic)
}

/// `Focus`：与 Tab 序同源的 `focusable` 校验。
fn on_focus(app: &mut App, window: WindowId, node: crate::node::id::NodeId) -> bool {
    let focusable_node = app
        .window(window)
        .is_some_and(|window_state| focusable(window_state.tree(), node));
    focusable_node && app.set_focus(window, Some(node))
}

/// 把目标收敛到**滚动容器**（自身是容器就用自身，否则找最近的祖先容器）。
fn scroll_container(
    app: &App,
    window: WindowId,
    node: crate::node::id::NodeId,
) -> Option<crate::node::id::NodeId> {
    let tree = app.window(window)?.tree();
    if tree.scroll_slot(node).is_some() {
        return Some(node);
    }
    crate::interaction::scroll::scroll_ancestor(tree, node).map(|(container, _)| container)
}

/// 步进滚动（`ScrollUnit`：`Item` = 一行，`Page` = 一屏）。
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
        Some(ActionData::ScrollUnit(accesskit::ScrollUnit::Item)) => slot.line_height.max(1.0),
        _ => viewport.size.height.max(slot.line_height).max(1.0),
    };
    let delta = match action {
        A11yAction::ScrollUp => Point::new(0.0, -step),
        A11yAction::ScrollDown => Point::new(0.0, step),
        A11yAction::ScrollLeft => Point::new(-step, 0.0),
        A11yAction::ScrollRight => Point::new(step, 0.0),
        _ => Point::ZERO,
    };
    apply_scroll(app, window, container, delta)
}

/// `ScrollIntoView`：把目标矩形滚入视口（最小移动）。
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
            .map_or(crate::math::Rect::default(), |state| state.layout().bounds),
    );
    let view = crate::interaction::scroll::viewport(tree, container);
    let mut delta = Point::ZERO;
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

/// `SetScrollOffset`：直接写偏移。
fn scroll_to(
    app: &mut App,
    window: WindowId,
    node: crate::node::id::NodeId,
    offset: Point,
) -> bool {
    let Some(container) = scroll_container(app, window, node) else {
        return false;
    };
    let Some(handle) = app.scroll_handle(window, container) else {
        return false;
    };
    app.scroll_to(&handle, offset).is_ok()
}

/// 相对滚动 + a11y 子树失效（bounds 随偏移变化 ⇒ AT 必须重读几何）。
fn apply_scroll(
    app: &mut App,
    window: WindowId,
    container: crate::node::id::NodeId,
    delta: Point,
) -> bool {
    if delta.x == 0.0 && delta.y == 0.0 {
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
        math::{Point, Rect},
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
        action: AkAction,
        target: crate::node::id::NodeId,
        data: Option<ActionData>,
    ) -> ActionRequest {
        ActionRequest {
            action,
            target_tree: TreeId::ROOT,
            target_node: AkNodeId(crate::a11y::A11yNodeId::from_engine(target).get()),
            data,
        }
    }

    /// review P2-5：UIA `RangeValuePattern.SetValue` 发的是 `NumericValue(f64)`，
    /// 应用 handler 必须能拿到这个数值 —— 旧实现把它落成 `A11yActionData::None`。
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

        dispatch(
            &mut app,
            window,
            request(
                AkAction::SetValue,
                node,
                Some(ActionData::NumericValue(42.5)),
            ),
        );

        assert_eq!(seen.get(), 42.5, "handler 必须收到 UIA 传的数值");
        assert!(
            app.a11y_diagnostics().is_empty(),
            "有 handler 时不得记未处理诊断"
        );
    }

    /// review P2-5：`Increment` / `Decrement` 必须能映射到引擎动作（此前被静默忽略）。
    #[test]
    fn increment_and_decrement_are_routed_to_the_handler() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let count = Rc::new(Cell::new(0_u32));
        let counter = Rc::clone(&count);
        app.on_a11y_action(A11yAction::Increment, move |_ctx| {
            counter.set(counter.get() + 1);
        });
        dispatch(&mut app, window, request(AkAction::Increment, node, None));
        assert_eq!(count.get(), 1, "Increment 必须落到 handler");
    }

    #[test]
    fn click_programmatically_dispatches_the_declared_action() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 10.0, 10.0))
            .on_click(Action::custom("press"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let hits = Rc::new(Cell::new(0));
        let counter = Rc::clone(&hits);
        app.on_action(Action::custom("press"), move |_ctx| {
            counter.set(counter.get() + 1);
        });

        dispatch(&mut app, window, request(AkAction::Click, node, None));

        assert_eq!(hits.get(), 1, "Click 必须落到 Action 表");
        assert!(app.a11y_diagnostics().is_empty());
    }

    #[test]
    fn focus_and_blur_move_the_focus_scope() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .on_click(Action::custom("press"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");

        dispatch(&mut app, window, request(AkAction::Focus, node, None));
        assert_eq!(app.focused(window), Some(node));

        dispatch(&mut app, window, request(AkAction::Blur, node, None));
        assert_eq!(app.focused(window), None);
    }

    /// a11y 动作在 `Dispatcher` 回投的任务里执行；`set_focus` 必须唤醒平台泵，否则焦点环
    /// 与 a11y 更新要等下一条无关消息（F-152）。
    #[test]
    fn a11y_focus_wakes_the_event_loop() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .on_click(Action::custom("press"))
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let _ = app.dispatcher().take_frame_wake();

        dispatch(&mut app, window, request(AkAction::Focus, node, None));

        assert_eq!(app.focused(window), Some(node));
        assert!(
            app.dispatcher().take_frame_wake(),
            "a11y Focus 必须唤醒事件循环（回投任务里改状态）"
        );
    }

    /// AR1b N2（P2 回归）：**模态作用域外的 a11y 动作一律不生效**。
    ///
    /// 失败面：只有 `Click`（经 `dispatch_action`）与 `Focus`（经 `set_focus`）过作用域，
    /// `SetValue` / `Increment` / `Custom` 直接跑应用闭包、`Scroll*` 直接 `scroll_*` ⇒
    /// 辅助技术在模态打开时仍能改背景控件状态、滚背景容器。修复点在 a11y 动作的唯一入口
    /// （本文件的 `dispatch`），判据与指针/键盘相同（模态子树 ∪ 属于该模态的浮层链）。
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
        // 背景里的滚动容器（AR1b：`Scroll*` 曾完全绕过作用域）。
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(100.0),
            height: taffy::style::Dimension::length(80.0),
        };
        inner.position = taffy::style::Position::Relative;
        let container = NodeBuilder::boxed()
            .style(crate::paint::style::Style {
                inner,
                clip: crate::paint::style::Clip::Bounds,
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("container");
        let handle = app
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.set_scroll_content(&handle, crate::math::Size::new(100.0, 400.0))
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

        // 模态对话框（作用域根）。
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

        // 模态外：SetValue / Increment / Scroll* 全部拒绝。
        dispatch(
            &mut app,
            window,
            request(
                AkAction::SetValue,
                background,
                Some(ActionData::Value("9".into())),
            ),
        );
        dispatch(
            &mut app,
            window,
            request(AkAction::Increment, background, None),
        );
        dispatch(
            &mut app,
            window,
            request(
                AkAction::ScrollDown,
                container,
                Some(ActionData::ScrollUnit(accesskit::ScrollUnit::Item)),
            ),
        );
        assert_eq!(set_value_hits.get(), 0, "模态外的 SetValue 不得生效");
        assert_eq!(increment_hits.get(), 0, "模态外的 Increment 不得生效");
        assert_eq!(
            app.scroll_state(&handle).expect("state").offset.y,
            0.0,
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

        // 作用域内：同一批动作照常落地（证明拦截的是位置，不是动作类型）。
        dispatch(
            &mut app,
            window,
            request(
                AkAction::SetValue,
                inside,
                Some(ActionData::Value("9".into())),
            ),
        );
        dispatch(&mut app, window, request(AkAction::Increment, inside, None));
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
        dispatch(&mut app, window, request(AkAction::Click, doomed, None));

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

        dispatch(
            &mut app,
            window,
            request(
                AkAction::SetValue,
                node,
                Some(ActionData::Value("42".into())),
            ),
        );
        assert_eq!(seen.take(), String::from("42"));
        assert!(
            app.a11y_diagnostics().is_empty(),
            "handler 落地后不再记 Unhandled"
        );
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

        dispatch(
            &mut app,
            window,
            request(
                AkAction::ReplaceSelectedText,
                node,
                Some(ActionData::Value("的".into())),
            ),
        );
        assert_eq!(seen.take(), String::from("的"));
    }

    #[test]
    fn set_text_selection_carries_character_offsets() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .a11y(A11yOverride {
                role: Some(crate::a11y::A11yRole::TextInput),
                ..A11yOverride::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        let seen = Rc::new(Cell::new((usize::MAX, usize::MAX)));
        let sink = Rc::clone(&seen);
        app.on_a11y_action(A11yAction::SetTextSelection, move |ctx| {
            if let A11yActionData::Selection { anchor, focus } = ctx.data() {
                sink.set((anchor, focus));
            }
        });

        dispatch(
            &mut app,
            window,
            request(
                AkAction::SetTextSelection,
                node,
                Some(ActionData::SetTextSelection(accesskit::TextSelection {
                    anchor: accesskit::TextPosition {
                        node: AkNodeId(crate::a11y::A11yNodeId::from_engine(node).get()),
                        character_index: 2,
                    },
                    focus: accesskit::TextPosition {
                        node: AkNodeId(crate::a11y::A11yNodeId::from_engine(node).get()),
                        character_index: 5,
                    },
                })),
            ),
        );
        assert_eq!(seen.get(), (2, 5));
    }

    #[test]
    fn scroll_unit_moves_the_container() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let mut inner = taffy::Style::<SmolStr>::DEFAULT;
        inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(100.0),
            height: taffy::style::Dimension::length(80.0),
        };
        inner.position = taffy::style::Position::Relative;
        let container = NodeBuilder::boxed()
            .style(crate::paint::style::Style {
                inner,
                clip: crate::paint::style::Clip::Bounds,
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("container");
        let handle = app
            .make_scroll(window, container, ScrollConfig::default())
            .expect("scroll");
        app.set_scroll_content(&handle, crate::math::Size::new(100.0, 400.0))
            .expect("content");

        dispatch(
            &mut app,
            window,
            request(
                AkAction::ScrollDown,
                container,
                Some(ActionData::ScrollUnit(accesskit::ScrollUnit::Item)),
            ),
        );
        let state = app.scroll_state(&handle).expect("state");
        assert!(state.offset.y > 0.0, "ScrollDown 必须下移偏移");
        assert!(app.a11y_diagnostics().is_empty(), "滚动必须被处理");
    }

    #[test]
    fn unmapped_action_is_diagnosed() {
        let (_runtime, mut app, window) = setup();
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("mount");
        dispatch(&mut app, window, request(AkAction::ShowTooltip, node, None));
        assert!(matches!(
            app.a11y_diagnostics(),
            [crate::a11y::A11yDiagnostic::UnhandledAction { .. }]
        ));
        let _ = (
            A11yPatch::default(),
            StylePatch::default(),
            ScrollConfig::default(),
            Point::ZERO,
        );
    }
}
