//! 指针 / 键盘 / 滚轮输入分发与指针会话（含拖拽、点击计数、悬停、捕获与光标请求）。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。
//!
//! ## 语义（R02 分区的修复点）
//!
//! - **只有主键产生 `click`、抢焦点**（R02-3）：次键（右键/中键/侧键）只更新 press 记录与
//!   hover，不派发 `ActionSource::Click`、不改焦点。
//! - **自动重复不触发绑定**（R02-2）：`repeat == true` 的按键只更新「按住」状态与默认导航
//!   行为，不查绑定表、不激活焦点控件 —— 长按 Enter 不会重复下单。
//! - **悬停可观测**（R02-4）：悬停节点变化时派生 [`Event::HoverChanged`]、给变化的节点标
//!   `PAINT` 并请求一帧；读点是 [`crate::app::App::hovered`]。
//! - **拖拽有启动阈值**（R02-9）：按下只登记候选，位移越过 `InputThresholds::drag_distance`
//!   才派发 `DragPhase::Start`（delta 用真实位移）并取得指针捕获。
//! - **指针捕获是公开 API**（R02-8）：`App::capture_pointer` / `App::release_pointer`。
//! - **滚动带阶段/单位/二维 delta**（R02-10/11）；**事件时间戳**贯穿点击与拖拽判定（R02-14）。

use std::time::Instant;

use smallvec::SmallVec;

use super::{App, ClickRecord};
use crate::{
    event::{
        Action, ActionSource, Event, InputEvent, InputOutcome, KeyCode, KeyState, Modifiers,
        PointerButton, ScrollPhase, ScrollUnit,
    },
    interaction::{
        ActionCtx, DragInfo, DragPhase, DragSession, DragState, InputThresholds, PendingDrag,
        PressRecord, ScrollHandle, drag as drag_util, focus, overlay as overlay_util, scroll,
    },
    layout::hit_test::HitResult,
    math::Point,
    node::id::NodeId,
    paint::style::Cursor,
    window::{Window, WindowId},
};

/// 覆盖输入阈值（平台层用系统真值初始化；测试也可显式设置）。
/// 消费者：`App::set_input_thresholds`。
pub(crate) fn set_input_thresholds(app: &mut App, thresholds: InputThresholds) {
    app.thresholds = thresholds;
}

/// 程序性分发（测试 / 菜单桥 / a11y 的 `Click`）。返回是否被处理。
///
/// **受模态作用域约束**（R02-12）：目标节点在作用域外 ⇒ `false`（不派发）。菜单桥与 a11y 的
/// `Click` 走这里；a11y 的其它动作（`SetValue` / `Increment` / `Custom` / `Scroll*`）不经过
/// 本函数，它们在 a11y 入口（`crate::a11y::action::dispatch`）用**同一个** `focus::in_scope`
/// 判据校验 —— 三条路径（指针 / 键盘 / 辅助技术）因此口径一致（AR1b N2）。
///
/// 例外是**引擎级动作**（[`Action::FocusNext`] / [`Action::FocusPrev`] / [`Action::Dismiss`]）：
/// 它们作用于窗口级状态而不是激活目标节点，模态打开时仍必须可用（Esc 关浮层、Tab 遍历）。
///
/// 引擎内部路径（点击 / 拖拽 / 取消）不走本函数：它们直接调
/// [`dispatch_action_with_hit`]，因为指针在 `on_pointer_down` 已按作用域裁决过，而取消类
/// 事件必须无条件送达。
/// 消费者：`App::dispatch_action`。
pub(crate) fn dispatch_action(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    action: Action,
    source: ActionSource,
) -> bool {
    // 引擎级动作（关浮层 / 移动焦点）不是「激活某个节点」：它们作用于**窗口级状态**，
    // 因此不受模态作用域限制（否则模态打开时 `dispatch_action(root, Dismiss)` 会被拒绝，
    // 而 Esc 的默认绑定恰恰走这条语义）。`Custom` 是节点/应用动作，必须过作用域。
    if !matches!(
        action,
        Action::FocusNext | Action::FocusPrev | Action::Dismiss
    ) {
        let scope = app.modal_scope(window);
        let in_scope = app
            .windows
            .get(&window)
            .is_some_and(|target| focus::in_scope(target.tree(), node, scope));
        if !in_scope {
            return false;
        }
    }
    app.dispatch_action_with_hit(window, node, action, source, None, Modifiers::empty())
}

/// 以当前焦点节点为 target 分发（无焦点 / 焦点在作用域外 ⇒ `false`）。
///
/// 作用域校验有两层：[`App::focused`] 已把作用域外的焦点过滤成 `None`，[`dispatch_action`]
/// 再做一次显式校验（R02-12：背景按钮不得被 Enter/Space 或 AT 激活）。
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
    if let Some(handler) = app.handlers.remove(&action) {
        // R01-12：条目在调用期间被 `remove` 出来，必须**任何退出路径**都放回：
        //
        // 1. panic 安全：旧实现在 `handler(&mut ctx)` 之后才 `insert`，handler 自身 panic 时闭包
        //    随 unwind 被 drop ⇒ 该动作的处理器永久丢失（下一次分发静默退化为默认行为）。
        // 2. 重入覆盖：handler 体内 `on_action(同一个 action, 新闭包)` 注册的条目不得被旧闭包覆盖
        //    —— 旧实现无条件 `insert`，把应用刚注册的新处理器踩掉。
        //
        // 做法与 a11y 那一侧（`a11y::action::HandlerGuard`，R13-20）同一套：返回守卫的 `Drop`
        // 负责放回，`or_insert` 只在 vacant 时写，于是"内层新注册的胜出"。
        let mut guard = HandlerGuard {
            app,
            action,
            handler: Some(handler),
        };
        {
            let mut ctx = ActionCtx::new(guard.app, window, node, hit, modifiers, source);
            if let Some(handler) = guard.handler.as_mut() {
                handler(&mut ctx);
            }
        }
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
    // E17：记下「最新指针位置」（`Window::pointer_position` 的唯一写入点）。
    //
    // 纯记账：不标脏、不请求帧（I3）；悬停 / 光标 / 拖拽仍按各自路径派生。带窗口逻辑坐标的
    // 指针事件都算 —— `PointerLeave` 自带离开点（三端都提供），因此离开后保留的最后值就是
    // 离开点；`Scroll` 的位置同样是指针位置。触摸 / 手势（`Magnify` / `Rotate` /
    // `SmartMagnify`）没有指针语义，不更新。
    let position = match &event {
        InputEvent::PointerEnter { position, .. }
        | InputEvent::PointerMove { position, .. }
        | InputEvent::PointerDown { position, .. }
        | InputEvent::PointerUp { position, .. }
        | InputEvent::PointerLeave { position, .. }
        | InputEvent::Scroll { position, .. } => Some(*position),
        _ => None,
    };
    if let Some(position) = position
        && let Some(target) = app.windows.get_mut(&window)
    {
        target.note_pointer_position(position);
    }
    app.validate_focus(window);
    match event {
        // 进入客户区与移动走同一条路径：两者都要重算命中、悬停与光标。区别只在**应用侧**：
        // `PointerEnter` 让观测者能区分「刚刚进入」与「在窗口内移动」。
        InputEvent::PointerEnter {
            position,
            modifiers,
            ..
        }
        | InputEvent::PointerMove {
            position,
            modifiers,
            ..
        } => app.on_pointer_move(window, position, modifiers, &mut outcome),
        InputEvent::PointerDown {
            position,
            button,
            modifiers,
            timestamp,
        } => app.on_pointer_down(window, position, button, modifiers, timestamp, &mut outcome),
        InputEvent::PointerUp {
            position,
            button,
            modifiers,
            timestamp,
        } => app.on_pointer_up(window, position, button, modifiers, timestamp, &mut outcome),
        InputEvent::PointerLeave { modifiers, .. } => app.on_pointer_leave(window, modifiers),
        // 系统取消指针会话（捕获丢失 / 系统弹窗夺走输入）：与失焦同一套收尾，但**不**动
        // 焦点与 a11y 焦点态 —— 焦点没有变，只是这次指针交互作废。
        InputEvent::PointerCancel { .. } => app.cancel_pointer_session(window),
        InputEvent::Scroll {
            position,
            delta,
            unit,
            phase,
            modifiers,
            ..
        } => app.on_scroll(
            window,
            position,
            delta,
            unit,
            phase,
            modifiers,
            &mut outcome,
        ),
        InputEvent::Key {
            code,
            state,
            repeat,
            modifiers,
            ..
        } => app.on_key(window, code, state, repeat, modifiers, &mut outcome),
        // 手势（捏合 / 旋转 / 智能缩放，R09-4）：引擎**不发明**默认语义（缩放倍率、旋转锚点、
        // 复位策略全是应用领域知识），因此这里只做交互收尾：手势属于指针会话，取消拖拽/悬停
        // 状态由平台侧的 `PointerCancel` 负责，本事件本身不改变引擎状态。应用通过
        // `App::drain_events` 观察它们（与 `Text` 同一条观测通路）。**当前只有 macOS 后端
        // 产生这些事件**（Windows / Linux 无手势生产点，见 `event::GesturePhase`）。
        InputEvent::Magnify { .. }
        | InputEvent::Rotate { .. }
        | InputEvent::SmartMagnify { .. } => {}
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
    timestamp: Instant,
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
    // ① 浮层外部点击（优先于焦点：否则会同时关浮层 + 触发下层按钮）。
    let outside = app.windows.get(&window).and_then(|target| {
        let tree = target.tree();
        overlay_util::outside_click_target(tree, tree.root(), hit.map(|result| result.node))
    });
    if let Some((node, _)) = outside {
        // 关闭浮层的这一次按下必须**整体消费**：抬起时不得再派发给下层。
        if let Some(target) = app.windows.get_mut(&window) {
            target.pointer_mut().press = None;
            target.pointer_mut().pending_drag = None;
        }
        app.remove_node(window, node);
        return;
    }
    // ② 模态浮层的焦点作用域（R02-12）：作用域外的按下整体消费 —— 不抢焦点、不激活下层
    //    动作（Blink `<dialog showModal>` 的 `inert` 背景、Qt 对话框的作用域同义）。
    //
    //    作用域 = 模态子树 **∪ 属于该模态的浮层链**：`create_overlay` 恒把浮层挂 root，所以
    //    对话框里的下拉/菜单必然落在模态子树之外；它们的 `opener` 在模态内（可嵌套递归），
    //    因此仍属于可交互范围（`in_modal_scope`）。旧实现只认子树 ⇒ 菜单项永远点不动。
    if let Some(scope) = app.modal_scope(window) {
        let inside = app.windows.get(&window).is_some_and(|target| {
            hit.is_some_and(|result| {
                overlay_util::in_modal_scope(target.tree(), result.node, Some(scope))
            })
        });
        if !inside {
            return;
        }
    }
    // ③ 记录按下（点击判定与拖拽抑制的输入）。
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        pointer.press = Some(PressRecord {
            node: hit.map(|result| result.node),
            position,
            button,
            at: timestamp,
        });
    }
    update_hover(app, window, hit.map(|result| result.node));
    // ④ 焦点：只有**主键**抢焦点（R02-3）。右键唤起上下文菜单不该改变焦点。
    if button.is_primary() {
        let focus_target = app.windows.get(&window).and_then(|target| {
            hit.and_then(|result| focus::nearest_focusable(target.tree(), result.node))
        });
        app.set_focus(window, focus_target);
    }
    // ⑤ 拖拽候选：命中链上第一条 `on_drag`。此时**不**派发 `DragPhase::Start`，也不捕获指针
    //    —— 位移越过阈值才转正（R02-9）。**只有主键**登记候选：次键按下不改变交互状态
    //    （R02-3；否则右键也能拖，与「只有主键走 click」自相矛盾）。
    if button.is_primary() {
        let drag = app.windows.get(&window).and_then(|target| {
            hit.and_then(|result| focus::drag_target(target.tree(), result.node))
        });
        if let Some((node, action)) = drag
            && let Some(target) = app.windows.get_mut(&window)
        {
            target.pointer_mut().pending_drag = Some(PendingDrag {
                node,
                action,
                origin: position,
            });
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
    let thresholds = app.thresholds;
    // 拖拽候选是否转正（越阈值的首次移动）。取出待派发的 `Start` 与进行中的 `Move`。
    let mut promote: Option<(NodeId, Action, Point)> = None;
    let mut moved: Option<(NodeId, Action, Point)> = None;
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        if let Some(session) = pointer.drag.as_mut() {
            session.current = position;
            session.phase = DragPhase::Move;
            moved = Some((session.node, session.action.clone(), session.delta()));
        } else if let Some(pending) = pointer.pending_drag.as_ref() {
            let dx = position.x - pending.origin.x;
            let dy = position.y - pending.origin.y;
            if drag_util::crosses_threshold(Point::new(dx, dy), thresholds.drag_distance) {
                let pending = pointer.pending_drag.take().expect("刚检查过");
                pointer.drag = Some(DragSession {
                    node: pending.node,
                    action: pending.action.clone(),
                    origin: pending.origin,
                    current: position,
                    phase: DragPhase::Start,
                });
                pointer.capture = Some(pending.node);
                promote = Some((pending.node, pending.action, Point::new(dx, dy)));
            }
        }
    }
    update_hover(app, window, hit.map(|result| result.node));
    update_cursor(app, window, hit);
    if let Some((node, action, delta)) = promote {
        // 转正：取得指针捕获（Windows `SetCapture`；macOS/Wayland 是平台隐式捕获）。
        app.capture_requests.insert(window, true);
        if app.dispatch_action_with_hit(
            window,
            node,
            action.clone(),
            ActionSource::Drag {
                phase: DragPhase::Start,
                delta,
            },
            hit,
            modifiers,
        ) {
            outcome.action = Some(action);
            outcome.target = Some(node);
        }
        return;
    }
    if let Some((node, action, delta)) = moved
        && app.dispatch_action_with_hit(
            window,
            node,
            action.clone(),
            ActionSource::Drag {
                phase: DragPhase::Move,
                delta,
            },
            hit,
            modifiers,
        )
    {
        outcome.action = Some(action);
        outcome.target = Some(node);
    }
}

/// 消费者：`App::on_pointer_up`。
pub(crate) fn on_pointer_up(
    app: &mut App,
    window: WindowId,
    position: Point,
    button: PointerButton,
    modifiers: Modifiers,
    timestamp: Instant,
    outcome: &mut InputOutcome,
) {
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    // 拖拽会话只由**起始按键**结束（R02-3）：左键拖拽进行中，右键抬起既不得 `end_drag`
    // 也不得清掉待定候选（Win32 的 `WM_LBUTTONUP` / `WM_RBUTTONUP` 是两条独立消息，
    // Qt / Blink 同样按按键配对）。次键抬起走下面的 press 分支（且不匹配 ⇒ 直接返回）。
    let session = app.windows.get_mut(&window).and_then(|target| {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        pointer.last_hit = hit;
        if !button.is_primary() {
            return None;
        }
        pointer.pending_drag = None;
        pointer.end_drag()
    });
    if let Some(session) = session {
        app.capture_requests.insert(window, false);
        // 拖拽结束：按下记录随之作废（否则下一次无按下事件的抬起会误判成点击）。
        if let Some(target) = app.windows.get_mut(&window) {
            target.pointer_mut().press = None;
        }
        let delta = session.delta();
        let action = session.action;
        if app.dispatch_action_with_hit(
            window,
            session.node,
            action.clone(),
            ActionSource::Drag {
                phase: DragPhase::End,
                delta,
            },
            hit,
            modifiers,
        ) {
            outcome.action = Some(action);
            outcome.target = Some(session.node);
        }
        return;
    }
    // 只取**同一按键**的按下记录：次键抬起不得清掉主键的按下（否则主键随后抬起不再产生
    // click，多键并按时表现为「点了没反应」）。
    let press = app.windows.get_mut(&window).and_then(|target| {
        let pointer = target.pointer_mut();
        if pointer.press.is_some_and(|press| press.button == button) {
            pointer.press.take()
        } else {
            None
        }
    });
    let Some(press) = press else {
        return;
    };
    // R02-3：只有主键产生 click。次键在抬起时**不**走点击判定（Win32 只有 `WM_LBUTTONUP`
    // 对应 `BN_CLICKED`、AppKit 的 target/action 只挂左键、Qt 的 `clicked()` 只认左键、
    // Blink 的 `click` 同样只由主键产生）。
    if !button.is_primary() {
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
    let count = app.click_count(window, press_node, position, button, timestamp);
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
            ActionSource::Click {
                click_count: count,
                button,
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

/// 消费者：`App::on_pointer_leave`。
pub(crate) fn on_pointer_leave(app: &mut App, window: WindowId, modifiers: Modifiers) {
    let mut changed = false;
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.modifiers = modifiers;
        let capture = pointer.capture.is_some();
        if !capture {
            pointer.press = None;
            pointer.pending_drag = None;
        }
        if pointer.cursor != Cursor::Default {
            pointer.cursor = Cursor::Default;
            changed = true;
        }
    }
    // 离开客户区 ⇒ 悬停链全部离开（W3C pointerleave；离开点由平台事件自己携带给观察者）。
    update_hover(app, window, None);
    if changed {
        app.cursor_pending = Some((window, Cursor::Default));
    }
}

/// 消费者：`App::on_scroll`。
pub(crate) fn on_scroll(
    app: &mut App,
    window: WindowId,
    position: Point,
    delta: Point,
    unit: ScrollUnit,
    phase: ScrollPhase,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    let hit = app.hit_at(window, position);
    outcome.hit = hit;
    let Some(result) = hit else {
        return;
    };
    // 模态作用域（R02-12）：作用域外的滚动整体消费 —— `inert` 的背景不得被滚轮/触控板滚动
    //（与按下同一条判定，含「属于该模态的弹出层」）。
    if let Some(scope) = app.modal_scope(window) {
        let inside = app.windows.get(&window).is_some_and(|target| {
            overlay_util::in_modal_scope(target.tree(), result.node, Some(scope))
        });
        if !inside {
            return;
        }
    }
    // Shift 交换两轴：各平台的约定都是「Shift + 滚轮 = 横向」。交换后每个容器只消费
    // **自己那条轴**的 delta，另一轴原样留给外层的容器（P2-4）。
    let (delta_x, delta_y) = if modifiers.contains(Modifiers::SHIFT) {
        (delta.y, delta.x)
    } else {
        (delta.x, delta.y)
    };
    if delta_x == 0.0 && delta_y == 0.0 {
        return;
    }
    // 连续设备的阶段（触控板 / 系统惯性）1:1 跟随；离散滚轮走平滑动画（R02-10）。
    //
    // `unit == Line` 时 delta 已经是**行**（平台按 `lines_per_tick` 折算过了），
    // `unit == Pixel` 时就是逻辑像素 —— 两者都只需容器自己的 `line_height` 做行→像素换算。
    let direct = unit == ScrollUnit::Pixel || phase.is_continuous();
    // 由内到外收集滚动祖先（含命中节点自身）：滚轮先给最内层，滚不动才交给外层，
    // 这就是浏览器的 scroll chaining。旧实现只取**一个**最近祖先，内层到底后滚轮
    // 直接失效（复杂图表里嵌套的横向 + 纵向容器最明显）。
    let Some(target) = app.windows.get(&window) else {
        return;
    };
    let mut chain: SmallVec<[(NodeId, crate::interaction::ScrollSlot, crate::math::Size); 8]> =
        SmallVec::new();
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
    let mut dx = delta_x;
    let mut dy = delta_y;
    for (node, slot, size) in chain {
        let drives = match slot.axis {
            crate::interaction::ScrollAxis::Vertical => dy,
            crate::interaction::ScrollAxis::Horizontal => dx,
        };
        if drives == 0.0 {
            continue;
        }
        // 本容器实际能消费的量（受 max_offset 夹紧）；吃不完的留给外层。
        //
        // 基准是**动画目标**而不是当前 offset：同一帧内的第二个滚轮事件必须累加在第一个
        // 事件的落点上，否则两次事件用同一个 `from` 重算目标，前一次的 delta 被静默丢弃
        // （触控板 / 高精度滚轮一个泵批次里常发多个事件，表现为「滚不动」）。
        let max = scroll::max_offset(&slot, size);
        let pending = if direct {
            // 精确滚动（触控板 / 惯性）立即接管：先取消本槽上的平滑动画，基准用**当前**偏移，
            // 否则动画目标与手指位移会互相打断（R02-10）。
            let handle = ScrollHandle::new(window, node);
            super::animation::cancel_scroll_animation(app, &handle);
            slot.offset
        } else {
            app.scroll_animation_target(node, window)
                .unwrap_or(slot.offset)
        };
        let (before, limit) = match slot.axis {
            crate::interaction::ScrollAxis::Vertical => (pending.y, max.y),
            crate::interaction::ScrollAxis::Horizontal => (pending.x, max.x),
        };
        // 基准必须钳在**当前**可滚范围内：动画 `to` 不会随内容收缩 / 视口变大而重定向
        // （`clamp_scroll_offsets` 只改 offset）。不钳的话 `before` 可能是越界值，
        // `consumed = (before - after) / line_height` 会放大成十几行，把**反向**残量
        // 传给外层容器（内层缩内容后向上滚一格，外层却往下滚；第二轮复审 P2-2）。
        let before = before.clamp(0.0, limit);
        let step = if unit == ScrollUnit::Line {
            slot.line_height
        } else {
            1.0
        };
        let after = (before - drives * step).clamp(0.0, limit);
        let consumed = (before - after) / step;
        if consumed == 0.0 {
            continue;
        }
        let handle = ScrollHandle::new(window, node);
        let mut goal = Point::new(0.0, 0.0);
        match slot.axis {
            crate::interaction::ScrollAxis::Vertical => goal.y = after,
            crate::interaction::ScrollAxis::Horizontal => goal.x = after,
        }
        if direct {
            let _ = app.scroll_to(&handle, goal);
        } else {
            let _ = app.scroll_to_animated(&handle, goal);
        }
        match slot.axis {
            crate::interaction::ScrollAxis::Vertical => dy -= consumed,
            crate::interaction::ScrollAxis::Horizontal => dx -= consumed,
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
    code: KeyCode,
    state: KeyState,
    repeat: bool,
    modifiers: Modifiers,
    outcome: &mut InputOutcome,
) {
    // ① 「按住集合」（R02-16）：`KeyState::Up` 不再被丢弃 —— 应用可以直接查
    //    [`crate::app::App::key_down`] 实现「按住连续」语义。`Unidentified` 也进集合：
    //    它可能对应一个真实按下的键，只是引擎不认识。
    //
    //    集合是**每窗口**的：平台把按键投递给拥有键盘焦点的窗口，`key_down(window, code)`
    //    必须只回答该窗口的状态（旧实现全 App 一份 ⇒ A 窗按住的键在 B 窗查询也为真）。
    match state {
        KeyState::Down => {
            app.held_keys.entry(window).or_default().insert(code);
        }
        KeyState::Up => {
            if let Some(held) = app.held_keys.get_mut(&window) {
                held.remove(&code);
            }
            return;
        }
    }
    // 自动重复（R02-2）：**绑定与激活都忽略**重复 —— 操作系统把重复定义为输入法/文本/导航的
    // 便利机制，而不是「重新按下」。旧实现对每个 Down 都查绑定，长按 Enter 会重复下单。
    if !repeat {
        // ② 焦点激活：Enter / Space 走「点击」的同一条冒泡（没声明就落到 ③，不吞键）。
        //
        //    作用域校验（R02-12）：`App::focused` 已把模态作用域外的焦点过滤成 `None`，因此
        //    背景按钮收不到 Enter；这里再显式校验一次作用域，防止焦点在两次读取之间变化。
        if matches!(code, KeyCode::Enter | KeyCode::NumpadEnter | KeyCode::Space) {
            let focused = app.focused(window);
            let scope = app.modal_scope(window);
            let activation = app.windows.get(&window).and_then(|target| {
                focused
                    .filter(|node| focus::in_scope(target.tree(), *node, scope))
                    .and_then(|node| focus::action_target(target.tree(), node))
            });
            if let Some((node, action)) = activation {
                let handled = app.dispatch_action_with_hit(
                    window,
                    node,
                    action.clone(),
                    ActionSource::Click {
                        click_count: 1,
                        button: PointerButton::Primary,
                    },
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
        // ③a 焦点作用域化绑定（E8）：只有本窗口焦点落在 `scope` 子树内时才命中并消费；
        //     焦点不在作用域内时**不消费**，继续走 ③b 全局绑定与 ④ 默认键盘行为。
        //     这是「树的方向键只在自己聚焦时吃掉」与「文本框编辑键」的机制；放在全局绑定
        //     之前，因此实例级绑定优先于应用级快捷键。
        //
        //     嵌套 scope：从焦点节点向上到根逐个 scope 匹配，**由内向外**尝试；第一个
        //     `Action` 有 handler 且分派成功的消费按键。全部未命中（或无 handler）则回落
        //     全局绑定 —— 内层显式不处理时外层实例仍有机会，语义与事件冒泡一致。
        let focused = app.focused(window);
        let mut scoped_actions: SmallVec<[Action; 4]> = SmallVec::new();
        if let Some(focused) = focused
            && let Some(tree) = app.windows.get(&window).map(Window::tree)
        {
            let mut cursor = Some(focused);
            while let Some(scope) = cursor {
                if let Some(action) = app
                    .scoped_bindings
                    .iter()
                    .find(|scoped| {
                        scoped.window == window
                            && scoped.scope == scope
                            && scoped.binding.key == code
                            && scoped.binding.modifiers == modifiers
                    })
                    .map(|scoped| scoped.binding.action.clone())
                {
                    scoped_actions.push(action);
                }
                cursor = tree.get(scope).ok().and_then(|target| target.parent());
            }
        }
        if let Some(node) = focused {
            for action in scoped_actions {
                if app.dispatch_action_with_hit(
                    window,
                    node,
                    action.clone(),
                    ActionSource::Key,
                    None,
                    modifiers,
                ) {
                    outcome.action = Some(action);
                    outcome.target = Some(node);
                    return;
                }
            }
        }
        // ③b 全局绑定（精确匹配 key + modifiers）。
        //
        //    绑定是**应用级**语义（`Ctrl+S`、`Tab`、`Esc`），不是「激活某个节点」：它不受模态
        //    作用域限制（对话框里仍要能按 Esc 关闭、Tab 遍历模态内控件）。target 只是
        //    `ActionCtx::node` 的落点，因此直接走 `dispatch_action_with_hit`，绕开
        //    `dispatch_action` 的作用域校验。
        let binding = app
            .bindings
            .iter()
            .find(|binding| binding.key == code && binding.modifiers == modifiers)
            .map(|binding| binding.action.clone());
        if let Some(action) = binding {
            let node =
                focused.or_else(|| app.windows.get(&window).map(|target| target.tree().root()));
            let Some(node) = node else {
                return;
            };
            if app.dispatch_action_with_hit(
                window,
                node,
                action.clone(),
                ActionSource::Key,
                None,
                modifiers,
            ) {
                outcome.action = Some(action);
                outcome.target = Some(node);
            }
            return;
        }
    }
    // ④ 默认键盘行为（R02-16）：焦点落在滚动容器里时方向键/翻页键滚动它。放在绑定之后，
    //    因此应用可以用自己的绑定**拦截**任意一个（「可被应用拦截」）。
    if modifiers.is_empty() {
        default_key_scroll(app, window, code);
    }
}

/// 焦点节点的默认键盘滚动（R02-16）。
///
/// 语义与 `QAbstractScrollArea::keyPressEvent`、`GtkScrolledWindow` 的键盘处理一致：
/// 方向键一行（容器的 `line_height`）、PageUp/PageDown 一页（视口）、Home/End 到两端。
/// 只动**容器自己声明的轴**；没有滚动祖先时什么也不做（不吞键）。
///
/// 判定用**逻辑键**（[`crate::event::Key`]）而不是物理码：默认导航关心的是「语义上是方向键/
/// 翻页键」，这正是逻辑层存在的理由（`NumpadEnter` 与 `Enter` 同义、左右修饰键合并）。
fn default_key_scroll(app: &mut App, window: WindowId, code: KeyCode) -> bool {
    use crate::{event::Key, interaction::ScrollAxis};
    let Some(key) = Key::from_code(code) else {
        return false;
    };
    let Some(focused) = app.focused(window) else {
        return false;
    };
    let Some(target) = app.windows.get(&window) else {
        return false;
    };
    let Some((node, slot)) = scroll::scroll_ancestor(target.tree(), focused) else {
        return false;
    };
    let viewport = scroll::viewport(target.tree(), node).size;
    let max = scroll::max_offset(&slot, viewport);
    let limit = match slot.axis {
        ScrollAxis::Vertical => max.y,
        ScrollAxis::Horizontal => max.x,
    };
    let line = slot.line_height;
    let page = match slot.axis {
        ScrollAxis::Vertical => viewport.height,
        ScrollAxis::Horizontal => viewport.width,
    };
    // 目标位置（`None` = 该键对本容器没有默认行为）。
    let goal = match (slot.axis, key) {
        (ScrollAxis::Vertical, Key::ArrowUp) => Some(-line),
        (ScrollAxis::Vertical, Key::ArrowDown) => Some(line),
        (ScrollAxis::Horizontal, Key::ArrowLeft) => Some(-line),
        (ScrollAxis::Horizontal, Key::ArrowRight) => Some(line),
        (_, Key::PageUp) => Some(-page),
        (_, Key::PageDown) => Some(page),
        (_, Key::Home) => Some(-limit),
        (_, Key::End) => Some(limit),
        _ => None,
    };
    let Some(delta) = goal else {
        return false;
    };
    let handle = ScrollHandle::new(window, node);
    let mut movement = Point::new(0.0, 0.0);
    match slot.axis {
        ScrollAxis::Vertical => movement.y = delta,
        ScrollAxis::Horizontal => movement.x = delta,
    }
    app.scroll_by(&handle, movement).is_ok()
}

/// 消费者：`App::click_count`。
///
/// 连击判定基准是**事件时间戳**（R02-14）与按键（R02-23 的共享部分）：`(窗口, 节点, 按键)`
/// 三元组一致、时间与位移都在阈值内才累加。左右键交替点击不再被算成双击。
pub(crate) fn click_count(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    position: Point,
    button: PointerButton,
    timestamp: Instant,
) -> u8 {
    let count = match app.last_click {
        Some(last)
            if last.window == window
                && last.node == Some(node)
                && last.button == button
                && timestamp.saturating_duration_since(last.at)
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
        button,
        position,
        at: timestamp,
        count,
    });
    count
}

/// 消费者：`App::on_window_focus_lost`。
pub(crate) fn on_window_focus_lost(app: &mut App, window: WindowId) {
    // 失焦期间抬起消息不再到达本窗口：该窗口的按住集合必须清空，否则 `key_down` 会永久为真
    //（与 `TranslateState::reset_transient` 对修饰键的处理同源）。其它窗口的集合不受影响
    // （集合是每窗口的，R02-16）。
    app.held_keys.remove(&window);
    // B6/B9 用户裁决「焦点移走/窗口失焦统一清 caret hint」：失焦后旧 hint 不再有意义，
    // 保留它会让重新获焦时平台按旧位置重开候选窗（三端同语义；平台层另有即时清场，
    // 这里是 App 层兜底，`clear_ime_cursor_request` 同值 no-op）。清掉后唤醒平台泵让
    // `sync_ime_cursor_area` 把空 hint 应用下去。
    if app
        .windows
        .get_mut(&window)
        .is_some_and(Window::clear_ime_cursor_request)
    {
        app.dispatcher.wake();
    }
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
    // 捕获可能只由 `capture_pointer` 置位（没有拖拽会话）：无论是否在拖拽，会话作废都必须
    // 请求平台释放捕获（R02-8：`SetCapture` 不释放会让窗口外的事件继续进来）。
    let had_capture = app
        .windows
        .get(&window)
        .is_some_and(|target| target.pointer().capture.is_some());
    let cancelled = app
        .windows
        .get_mut(&window)
        .and_then(|target| target.pointer_mut().end_drag());
    if cancelled.is_some() || had_capture {
        app.capture_requests.insert(window, false);
    }
    if let Some(session) = cancelled {
        let delta = session.delta();
        // 取消必须**无条件**送达（`dispatch_action` 会拒绝作用域外目标；模态恰好在拖拽中
        // 打开时，取消事件仍要落到原拖拽节点上，否则应用永远收不到 `DragPhase::Cancel`）。
        let _ = app.dispatch_action_with_hit(
            window,
            session.node,
            session.action,
            ActionSource::Drag {
                phase: DragPhase::Cancel,
                delta,
            },
            None,
            Modifiers::empty(),
        );
    }
    if let Some(target) = app.windows.get_mut(&window) {
        let pointer = target.pointer_mut();
        pointer.press = None;
        pointer.pending_drag = None;
        pointer.cursor = Cursor::Default;
    }
    update_hover(app, window, None);
}

// ── 悬停（R02-4） ──────────────────────────────────────────────────────────

/// 从 `node` 到根的祖先链（由外到内）。悬停链就是 CSS `:hover` 的作用集合。
fn hover_chain(tree: &crate::node::tree::NodeTree, node: Option<NodeId>) -> SmallVec<[NodeId; 8]> {
    let mut chain = SmallVec::new();
    let mut cursor = node;
    while let Some(current) = cursor {
        if tree.get(current).is_err() {
            break;
        }
        chain.push(current);
        cursor = tree.get(current).ok().and_then(|target| target.parent());
    }
    chain.reverse();
    chain
}

/// 更新悬停节点；有迁移时派生 [`Event::HoverChanged`]、标 `PAINT` 并请求一帧。
///
/// **受模态作用域约束**（E12）：作用域外的命中视为「没有悬停」。旧 `BlockMouse` 的 hitbox
/// 同时挡住命中与 hover；引擎的模态作用域必须给出同一语义，否则对话框打开时背景的
/// `:hover` 与 tooltip 仍会亮起、并画在对话框之上（P2-4b R07）。
fn update_hover(app: &mut App, window: WindowId, mut next: Option<NodeId>) {
    let (left, entered) = {
        let Some(target) = app.windows.get(&window) else {
            return;
        };
        let tree = target.tree();
        // 模态作用域外 ⇒ 无悬停；作用域内（模态子树 / 属于模态的弹出层）照常。
        let scope = overlay_util::focus_scope_root(tree, tree.root());
        next = next.filter(|node| {
            tree.get(*node).is_ok() && overlay_util::in_modal_scope(tree, *node, scope)
        });
        // 陈旧节点（已被卸载）视为「无悬停」：虚拟列表回收行之后旧 id 会留在槽里。
        let previous = target
            .pointer()
            .hovered
            .filter(|node| tree.get(*node).is_ok());
        if previous == next {
            return;
        }
        let old = hover_chain(tree, previous);
        let new = hover_chain(tree, next);
        let left: Vec<NodeId> = old
            .iter()
            .rev()
            .filter(|node| !new.contains(node))
            .copied()
            .collect();
        let entered: Vec<NodeId> = new
            .iter()
            .filter(|node| !old.contains(node))
            .copied()
            .collect();
        (left, entered)
    };
    if let Some(target) = app.windows.get_mut(&window) {
        target.pointer_mut().hovered = next;
        // 悬停是**样式输入**：变化节点必须标 `PAINT`，否则 `bind_style` 不会再求值，
        // 「悬停高亮」永远不会出现（旧实现只写槽、不标脏，R02-4 的根因）。
        //
        // 只标**挂了动态绑定**的节点：没有 `bind_style` / `bind_text` 的节点外观不随悬停变化，
        // 标脏只会白跑一次 lane 重建 —— 而悬停是高频事件，祖先链上尤其明显。
        let marked: SmallVec<[NodeId; 8]> = left
            .iter()
            .chain(entered.iter())
            .copied()
            .filter(|node| {
                target
                    .tree()
                    .get(*node)
                    .is_ok_and(|slot| slot.has_bindings())
            })
            .collect();
        for node in marked {
            // 悬停是**动态属性**输入：`BIND` 负责重算闭包，`PAINT` 负责重建 lane（R03-9 的两条通道）。
            target.tree_mut().mark_bind(node);
            target.tree_mut().mark_paint(node);
        }
    }
    app.events.push(
        window,
        Event::HoverChanged {
            hovered: next,
            left,
            entered,
        },
    );
    app.request_frame();
}

/// 模态作用域变化（模态浮层打开 / 关闭 / 提层）后重算悬停。
///
/// 作用域外的悬停必须**立刻结束**：应用可能用 `App::hovered` 驱动 tooltip / `:hover`
/// 样式，而指针没有移动就不会有新的 `PointerMove` —— 不重算的话背景 tooltip 会一直显示到
/// 用户移动鼠标（P2-4b R07 的「tooltip 画在模态之上」）。消费者：`create_overlay` /
/// `bring_overlay_to_front`（只在 `trap_focus` 浮层发生变化时调用）。
pub(crate) fn revalidate_hover(app: &mut App, window: WindowId) {
    let next = app
        .windows
        .get(&window)
        .and_then(|target| target.pointer().hovered);
    update_hover(app, window, next);
}

/// 悬停变化后重新解析光标（只有变化时才推给平台）。
fn update_cursor(app: &mut App, window: WindowId, hit: Option<HitResult>) {
    let cursor = app.windows.get(&window).map_or(Cursor::Default, |target| {
        let tree = target.tree();
        // 模态作用域外不解析指针形状：背景的手型/缩放光标不得在对话框上显示（与 hover 同源）。
        let scope = overlay_util::focus_scope_root(tree, tree.root());
        let hit = hit.filter(|result| overlay_util::in_modal_scope(tree, result.node, scope));
        focus::resolve_cursor(tree, hit.map(|result| result.node))
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

// ── 指针捕获（R02-8） ──────────────────────────────────────────────────────

/// 消费者：`App::capture_pointer`。
pub(crate) fn capture_pointer(app: &mut App, window: WindowId, node: NodeId) -> bool {
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    if target.tree().get(node).is_err() {
        return false;
    }
    if target.pointer().capture == Some(node) {
        return false;
    }
    target.pointer_mut().capture = Some(node);
    app.capture_requests.insert(window, true);
    true
}

/// 消费者：`App::release_pointer`。
pub(crate) fn release_pointer(app: &mut App, window: WindowId) -> bool {
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    if target.pointer_mut().capture.take().is_none() {
        return false;
    }
    app.capture_requests.insert(window, false);
    true
}

/// 当前捕获目标（`None` = 没有捕获 / 窗口不存在 / **捕获节点已卸载**）。
///
/// 与 [`hovered`](crate::app::App::hovered) / [`focused`](crate::app::App::focused) 同一纪律：
/// 读点过滤陈旧 id。捕获目标被应用卸载（虚拟列表回收、切换面板）时，`remove_node` 会主动
/// 释放并派发 `PointerCancel`；这里的校验是第二道防线（树被直接改写等路径）。
/// 消费者：`App::pointer_captured`。
#[must_use]
pub(crate) fn pointer_captured(app: &App, window: WindowId) -> Option<NodeId> {
    let target = app.windows.get(&window)?;
    let node = target.pointer().capture?;
    target.tree().get(node).is_ok().then_some(node)
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
    app.capture_requests.insert(window, false);
    let delta = session.delta();
    let action = session.action;
    // 取消必须无条件送达（同 `cancel_pointer_session`：作用域校验不得吞掉收尾事件）。
    let _ = app.dispatch_action_with_hit(
        window,
        session.node,
        action,
        ActionSource::Drag {
            phase: DragPhase::Cancel,
            delta,
        },
        None,
        Modifiers::empty(),
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
/// 每窗口**目标状态**（R02-8/R02-22）：旧实现是单槽 `Option<(WindowId, bool)>`，同一泵轮里
/// A 窗口的请求会被 B 覆盖丢失；多窗口下 `SetCapture` 是每窗口状态，覆盖就等于丢请求。
/// 消费者：`App::take_capture_requests`。
pub(crate) fn take_capture_requests(app: &mut App) -> Vec<(WindowId, bool)> {
    std::mem::take(&mut app.capture_requests)
        .into_iter()
        .collect()
}
/// 动作 handler 的归还守卫：`Drop` 保证**panic 也放回**表里（R01-12）。
///
/// 与 `a11y::action::HandlerGuard`（R13-20）同构：`take` 出来是为了在调用期把 `&mut App`
/// 借给 `ActionCtx`；放回用 `or_insert`，只在表里**没有**新条目时写 —— handler 体内对同一个
/// action 的重新注册（`App::on_action`）因此不会被旧闭包踩掉。
struct HandlerGuard<'a> {
    app: &'a mut App,
    action: Action,
    handler: Option<Box<dyn FnMut(&mut ActionCtx<'_>)>>,
}

impl Drop for HandlerGuard<'_> {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            self.app
                .handlers
                .entry(self.action.clone())
                .or_insert(handler);
        }
    }
}
