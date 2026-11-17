//! 指针 / 键盘 / 滚轮输入分发与指针会话（含拖拽、点击计数、光标与捕获请求）。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use smallvec::SmallVec;

use super::{App, ClickRecord};
use crate::{
    event::{
        Action, ActionSource, InputEvent, InputOutcome, Key, KeyState, Modifiers, PointerButton,
    },
    interaction::{
        ActionCtx, DragInfo, DragPhase, DragSession, DragState, InputThresholds, PressRecord,
        ScrollAxis, ScrollHandle, drag as drag_util, focus, overlay as overlay_util, scroll,
    },
    layout::hit_test::HitResult,
    math::{Point, Size},
    node::id::NodeId,
    paint::style::Cursor,
    window::WindowId,
};

/// 覆盖输入阈值（平台层用 `GetDoubleClickTime` / `SM_CXDRAG` 初始化；测试也可显式设置）。
/// 消费者：`App::set_input_thresholds`。
pub(crate) fn set_input_thresholds(app: &mut App, thresholds: InputThresholds) {
    app.thresholds = thresholds;
}

/// 程序性分发（测试 / 菜单桥 / a11y 的 `ActionRequest`）。返回是否被处理。
/// 消费者：`App::dispatch_action`。
pub(crate) fn dispatch_action(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    action: Action,
    source: ActionSource,
) -> bool {
    app.dispatch_action_with_hit(window, node, action, source, None, Modifiers::empty())
}

/// 以当前焦点节点为 target 分发（无焦点 ⇒ `false`）。
/// 消费者：`App::dispatch_action_focused`。
pub(crate) fn dispatch_action_focused(
    app: &mut App,
    window: WindowId,
    action: Action,
    source: ActionSource,
) -> bool {
    let Some(node) = app.focused(window) else {
        return false;
    };
    app.dispatch_action(window, node, action, source)
}

/// 分发的唯一实现（`hit` 只有指针路径会带；`modifiers` 由调用方显式给出 —— 键盘路径必须是
/// **本次按键**的修饰键，而不是指针状态的缓存）。
/// 消费者：`App::dispatch_action_with_hit`。
pub(crate) fn dispatch_action_with_hit(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    action: Action,
    source: ActionSource,
    hit: Option<HitResult>,
    modifiers: Modifiers,
) -> bool {
    let Some(target) = app.windows.get(&window) else {
        return false;
    };
    if target.tree().get(node).is_err() {
        return false;
    }
    if let Some(mut handler) = app.handlers.remove(&action) {
        {
            let mut ctx = ActionCtx::new(app, window, node, hit, modifiers, source);
            handler(&mut ctx);
        }
        app.handlers.insert(action, handler);
        return true;
    }
    match action {
        Action::FocusNext => {
            app.focus_next(window);
            true
        }
        Action::FocusPrev => {
            app.focus_prev(window);
            true
        }
        Action::Dismiss => app.dismiss_overlay(window),
        Action::Custom(_) => false,
    }
}

// ── P10：事件分发 ──────────────────────────────────────────────────────────

/// 分发一次输入事件（平台泵与 `TestApp::inject` 的**唯一**入口）。
/// 消费者：`App::dispatch_input`。
pub(crate) fn dispatch_input(app: &mut App, window: WindowId, event: InputEvent) -> InputOutcome {
    let mut outcome = InputOutcome::default();
    if !app.windows.contains_key(&window) {
        return outcome;
    }
    app.validate_focus(window);
    match event {
        InputEvent::PointerDown {
            position,
            button,
            modifiers,
        } => app.on_pointer_down(window, position, button, modifiers, &mut outcome),
        InputEvent::PointerMove {
            position,
            modifiers,
        } => {
            app.on_pointer_move(window, position, modifiers, &mut outcome);
        }
        InputEvent::PointerUp {
            position,
            button,
            modifiers,
        } => app.on_pointer_up(window, position, button, modifiers, &mut outcome),
        InputEvent::PointerLeave => app.on_pointer_leave(window),
        // 系统取消指针会话（捕获丢失 / 系统弹窗夺走输入）：与失焦同一套收尾，但**不**动
        // 焦点与 a11y 焦点态 —— 焦点没有变，只是这次指针交互作废。
        InputEvent::PointerCancel => app.cancel_pointer_session(window),
        InputEvent::Scroll {
            position,
            delta_x,
            delta_y,
            modifiers,
        } => app.on_scroll(window, position, delta_x, delta_y, modifiers, &mut outcome),
        InputEvent::Key {
            key,
            state,
            modifiers,
        } => app.on_key(window, key, state, modifiers, &mut outcome),
        InputEvent::Text { .. } => {}
    }
    outcome
}

/// 消费者：`App::hit_at`。
pub(crate) fn hit_at(app: &App, window: WindowId, position: Point) -> Option<HitResult> {
    app.windows
        .get(&window)
        .and_then(|target| target.tree().hit_test(position))
}

/// 消费者：`App::on_pointer_down`。
pub(crate) fn on_pointer_down(
    app: &mut App,
    window: WindowId,
    position: Point,
    button: PointerButton,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    if app
        .windows
        .get(&window)
        .is_some_and(|target| target.pointer().drag.is_some())
    {
        // 拖拽进行中：忽略二次按下（会话只由抬起 / 取消 / 失焦结束）。
        return;
    }
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        pointer.hover = hit.map(|result| result.node);
        pointer.press = Some(PressRecord {
            node: hit.map(|result| result.node),
            position,
            button,
            at: app.clock,
        });
    }
    // ① 浮层外部点击（优先于焦点：否则会同时关浮层 + 触发下层按钮）。
    let outside = app.windows.get(&window).and_then(|target| {
        let tree = target.tree();
        overlay_util::outside_click_target(tree, tree.root(), hit.map(|result| result.node))
    });
    if let Some((node, _)) = outside {
        // 关闭浮层的这一次按下必须**整体消费**：抬起时不得再派发给下层。
        if let Some(target) = app.windows.get_mut(&window) {
            target.pointer_mut().press = None;
        }
        app.remove_node(window, node);
        return;
    }
    // ② 焦点：命中链上最近的可聚焦祖先（没有 ⇒ 清空）。
    let focus_target = app.windows.get(&window).and_then(|target| {
        hit.and_then(|result| focus::nearest_focusable(target.tree(), result.node))
    });
    app.set_focus(window, focus_target);
    // ③ 拖拽：命中链上第一条 `on_drag`。
    let drag = app
        .windows
        .get(&window)
        .and_then(|target| hit.and_then(|result| focus::drag_target(target.tree(), result.node)));
    if let Some((node, action)) = drag {
        if let Some(target) = app.windows.get_mut(&window) {
            let pointer = target.pointer_mut();
            pointer.drag = Some(DragSession {
                node,
                action: action.clone(),
                origin: position,
                current: position,
                phase: DragPhase::Start,
            });
            pointer.capture = true;
        }
        app.capture_request = Some((window, true));
        let handled = app.dispatch_action_with_hit(
            window,
            node,
            action.clone(),
            ActionSource::Drag {
                phase: DragPhase::Start,
                delta: Point::ZERO,
            },
            hit,
            modifiers,
        );
        if handled {
            outcome.action = Some(action);
            outcome.target = Some(node);
        }
    }
}

/// 消费者：`App::on_pointer_move`。
pub(crate) fn on_pointer_move(
    app: &mut App,
    window: WindowId,
    position: Point,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    let session = app.windows.get_mut(&window).and_then(|target| {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        pointer.hover = hit.map(|result| result.node);
        pointer.drag.as_mut().map(|session| {
            session.current = position;
            session.phase = DragPhase::Move;
            session.clone()
        })
    });
    if let Some(session) = session {
        let delta = session.delta();
        let handled = app.dispatch_action_with_hit(
            window,
            session.node,
            session.action.clone(),
            ActionSource::Drag {
                phase: DragPhase::Move,
                delta,
            },
            hit,
            modifiers,
        );
        if handled {
            outcome.action = Some(session.action);
            outcome.target = Some(session.node);
        }
        return;
    }
    let cursor = app.windows.get(&window).map_or(Cursor::Default, |target| {
        focus::resolve_cursor(target.tree(), hit.map(|result| result.node))
    });
    let mut changed = false;
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        if pointer.cursor != cursor {
            pointer.cursor = cursor;
            changed = true;
        }
    }
    if changed {
        app.cursor_pending = Some((window, cursor));
    }
}

/// 消费者：`App::on_pointer_up`。
pub(crate) fn on_pointer_up(
    app: &mut App,
    window: WindowId,
    position: Point,
    button: PointerButton,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    let session = app.windows.get_mut(&window).and_then(|target| {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        pointer.end_drag()
    });
    if let Some(session) = session {
        app.capture_request = Some((window, false));
        let delta = session.delta();
        let action = session.action;
        let handled = app.dispatch_action_with_hit(
            window,
            session.node,
            action.clone(),
            ActionSource::Drag {
                phase: DragPhase::End,
                delta,
            },
            hit,
            modifiers,
        );
        if handled {
            outcome.action = Some(action);
            outcome.target = Some(session.node);
        }
        return;
    }
    let press = app
        .windows
        .get_mut(&window)
        .and_then(|target| target.pointer_mut().press.take());
    let Some(press) = press else {
        return;
    };
    if press.button != button {
        return;
    }
    let Some(press_node) = press.node else {
        return;
    };
    let moved = Point::new(position.x - press.position.x, position.y - press.position.y);
    if drag_util::crosses_threshold(moved, app.thresholds.drag_distance) {
        return;
    }
    if hit.map(|result| result.node) != Some(press_node) {
        return;
    }
    let count = app.click_count(window, press_node, position);
    outcome.click_count = count;
    let target_action = app
        .windows
        .get(&window)
        .and_then(|target| focus::action_target(target.tree(), press_node));
    if let Some((node, action)) = target_action {
        let handled = app.dispatch_action_with_hit(
            window,
            node,
            action.clone(),
            ActionSource::Click { click_count: count },
            hit,
            modifiers,
        );
        if handled {
            outcome.action = Some(action);
            outcome.target = Some(node);
        }
    }
}

/// 消费者：`App::on_pointer_leave`。
pub(crate) fn on_pointer_leave(app: &mut App, window: WindowId) {
    let mut changed = false;
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        let capture = pointer.capture;
        pointer.hover = None;
        if !capture {
            pointer.press = None;
        }
        if pointer.cursor != Cursor::Default {
            pointer.cursor = Cursor::Default;
            changed = true;
        }
    }
    if changed {
        app.cursor_pending = Some((window, Cursor::Default));
    }
}

/// 消费者：`App::on_scroll`。
pub(crate) fn on_scroll(
    app: &mut App,
    window: WindowId,
    position: Point,
    delta_x: f32,
    delta_y: f32,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    let Some(result) = hit else {
        return;
    };
    // Shift 交换两轴：各平台的约定都是「Shift + 滚轮 = 横向」。交换后每个容器只消费
    // **自己那条轴**的 delta，另一轴原样留给外层的容器（P2-4）。
    let (mut dx, mut dy) = if modifiers.contains(Modifiers::SHIFT) {
        (delta_y, delta_x)
    } else {
        (delta_x, delta_y)
    };
    if dx == 0.0 && dy == 0.0 {
        return;
    }
    // 由内到外收集滚动祖先（含命中节点自身）：滚轮先给最内层，滚不动才交给外层，
    // 这就是浏览器的 scroll chaining。旧实现只取**一个**最近祖先，内层到底后滚轮
    // 直接失效（复杂图表里嵌套的横向 + 纵向容器最明显）。
    let Some(target) = app.windows.get(&window) else {
        return;
    };
    let mut chain: SmallVec<[(NodeId, crate::interaction::ScrollSlot, Size); 8]> = SmallVec::new();
    let mut cursor = Some(result.node);
    while let Some(current) = cursor {
        if let Some(slot) = target.tree().scroll_slot(current) {
            let size = scroll::viewport(target.tree(), current).size;
            chain.push((current, slot, size));
        }
        cursor = target
            .tree()
            .get(current)
            .ok()
            .and_then(|node| node.parent());
    }
    for (node, slot, size) in chain {
        let drives = match slot.axis {
            ScrollAxis::Vertical => dy,
            ScrollAxis::Horizontal => dx,
        };
        if drives == 0.0 {
            continue;
        }
        // 本容器实际能消费的行数（受 max_offset 夹紧）；吃不完的留给外层。
        //
        // 基准是**动画目标**而不是当前 offset：同一帧内的第二个滚轮事件必须累加在第一个
        // 事件的落点上，否则两次事件用同一个 `from` 重算目标，前一次的 delta 被静默丢弃
        // （触控板 / 高精度滚轮一个泵批次里常发多个事件，表现为「滚不动」）。
        let max = scroll::max_offset(&slot, size);
        let pending = app
            .scroll_animation_target(node, window)
            .unwrap_or(slot.offset);
        let (before, limit) = match slot.axis {
            ScrollAxis::Vertical => (pending.y, max.y),
            ScrollAxis::Horizontal => (pending.x, max.x),
        };
        // 基准必须钳在**当前**可滚范围内：动画 `to` 不会随内容收缩 / 视口变大而重定向
        // （`clamp_scroll_offsets` 只改 offset）。不钳的话 `before` 可能是越界值，
        // `consumed = (before - after) / line_height` 会放大成十几行，把**反向**残量
        // 传给外层容器（内层缩内容后向上滚一格，外层却往下滚；第二轮复审 P2-2）。
        let before = before.clamp(0.0, limit);
        let after = (before - drives * slot.line_height).clamp(0.0, limit);
        let consumed = (before - after) / slot.line_height;
        if consumed == 0.0 {
            continue;
        }
        let handle = ScrollHandle::new(window, node);
        let mut target = Point::new(0.0, 0.0);
        match slot.axis {
            ScrollAxis::Vertical => target.y = after,
            ScrollAxis::Horizontal => target.x = after,
        }
        let _ = app.scroll_to_animated(&handle, target);
        match slot.axis {
            ScrollAxis::Vertical => dy -= consumed,
            ScrollAxis::Horizontal => dx -= consumed,
        }
        if dx == 0.0 && dy == 0.0 {
            break;
        }
    }
}

/// 消费者：`App::on_key`。
pub(crate) fn on_key(
    app: &mut App,
    window: WindowId,
    key: Key,
    state: KeyState,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    if state != KeyState::Down {
        return;
    }
    // ① 焦点激活：Enter / Space 走「点击」的同一条冒泡（没声明就落到 ②，不吞键）。
    if matches!(key, Key::Enter | Key::Space) {
        let focused = app.focused(window);
        let activation = app
            .windows
            .get(&window)
            .and_then(|target| focused.and_then(|node| focus::action_target(target.tree(), node)));
        if let Some((node, action)) = activation {
            let handled = app.dispatch_action_with_hit(
                window,
                node,
                action.clone(),
                ActionSource::Click { click_count: 1 },
                None,
                modifiers,
            );
            if handled {
                outcome.action = Some(action);
                outcome.target = Some(node);
            }
            return;
        }
    }
    // ② 全局绑定（精确匹配 key + modifiers）。
    let binding = app
        .bindings
        .iter()
        .find(|binding| binding.key == key && binding.modifiers == modifiers)
        .map(|binding| binding.action.clone());
    let Some(action) = binding else {
        return;
    };
    let node = app
        .focused(window)
        .or_else(|| app.windows.get(&window).map(|target| target.tree().root()));
    let Some(node) = node else {
        return;
    };
    if app.dispatch_action(window, node, action.clone(), ActionSource::Key) {
        outcome.action = Some(action);
        outcome.target = Some(node);
    }
}

/// 消费者：`App::click_count`。
pub(crate) fn click_count(app: &mut App, window: WindowId, node: NodeId, position: Point) -> u8 {
    let count = match app.last_click {
        Some(last)
            if last.window == window
                && last.node == Some(node)
                && app.clock.saturating_duration_since(last.at)
                    <= app.thresholds.double_click_time
                && drag_util::within(
                    last.position,
                    position,
                    app.thresholds.double_click_distance,
                ) =>
        {
            last.count.saturating_add(1)
        }
        _ => 1,
    };
    app.last_click = Some(ClickRecord {
        window,
        node: Some(node),
        position,
        at: app.clock,
        count,
    });
    count
}

/// 消费者：`App::on_window_focus_lost`。
pub(crate) fn on_window_focus_lost(app: &mut App, window: WindowId) {
    app.cancel_pointer_session(window);
}

/// 作废本窗口当前的指针会话：结束拖拽（派发 `DragPhase::Cancel`）、清按压/悬停/光标、
/// 释放指针捕获。
///
/// 两条触发路径共用：窗口失焦（`Event::FocusChanged { focused: false }`）与平台报告的
/// **指针会话取消**（`InputEvent::PointerCancel`：Windows 的 `WM_CAPTURECHANGED`、
/// macOS 的窗口失去 key、Linux 的指针 `leave`）。缺了这条路径，拖拽中 Alt+Tab 会让会话
/// 永久停在「按住」，用户只能重启应用。
/// 消费者：`App::cancel_pointer_session`。
pub(crate) fn cancel_pointer_session(app: &mut App, window: WindowId) {
    let cancelled = app
        .windows
        .get_mut(&window)
        .and_then(|target| target.pointer_mut().end_drag());
    if let Some(session) = cancelled {
        app.capture_request = Some((window, false));
        let delta = session.delta();
        let _ = app.dispatch_action(
            window,
            session.node,
            session.action,
            ActionSource::Drag {
                phase: DragPhase::Cancel,
                delta,
            },
        );
    }
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.hover = None;
        pointer.press = None;
        pointer.cursor = Cursor::Default;
    }
}

// ── P10：拖拽 ──────────────────────────────────────────────────────────────

/// 当前拖拽会话。
/// 消费者：`App::drag_state`。
#[must_use]
pub(crate) fn drag_state(app: &App, window: WindowId) -> Option<DragState> {
    let target = app.windows.get(&window)?;
    let session = target.pointer().drag.as_ref()?;
    Some(DragState::new(window, session.node))
}

/// 拖拽的只读快照。
/// 消费者：`App::drag`。
#[must_use]
pub(crate) fn drag(app: &App, state: &DragState) -> Option<DragInfo> {
    let target = app.windows.get(&state.window())?;
    let session = target.pointer().drag.as_ref()?;
    if session.node != state.node() {
        return None;
    }
    Some(DragInfo {
        origin: session.origin,
        current: session.current,
        delta: session.delta(),
        phase: session.phase,
    })
}

/// 取消拖拽（派发 `Cancel` 阶段并释放捕获）。
/// 消费者：`App::cancel_drag`。
pub(crate) fn cancel_drag(app: &mut App, state: &DragState) -> bool {
    let window = state.window();
    let session = app
        .windows
        .get_mut(&window)
        .and_then(|target| target.pointer_mut().end_drag());
    let Some(session) = session else {
        return false;
    };
    app.capture_request = Some((window, false));
    let delta = session.delta();
    let action = session.action;
    app.dispatch_action(
        window,
        session.node,
        action,
        ActionSource::Drag {
            phase: DragPhase::Cancel,
            delta,
        },
    );
    true
}

/// 取走待推给平台的光标变化（**带窗口 id**：光标是每窗口状态）。
/// 消费者：`App::take_cursor_change`。
pub(crate) fn take_cursor_change(app: &mut App) -> Option<(WindowId, Cursor)> {
    app.cursor_pending.take()
}

/// 取走待推给平台的捕获请求（`(窗口, true)` = 取得、`(窗口, false)` = 释放）。
///
/// 必须带窗口 id：`SetCapture` 是**每窗口**状态，固定发给首窗会让非首窗的拖拽拿不到
/// 捕获，鼠标移出窗口后松手就丢 `WM_LBUTTONUP`，会话永久卡在「按住」（复审 P1-3）。
/// 消费者：`App::take_capture_request`。
pub(crate) fn take_capture_request(app: &mut App) -> Option<(WindowId, bool)> {
    app.capture_request.take()
}
