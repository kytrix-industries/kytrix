//! 动画槽与引擎时钟：注册 / 推进 / 取消 / 完成通知，以及「是否需要持续出帧」。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。
//!
//! **E10 完成通知**：每次 `App::animate` 的动画终止（到达终值 / 取消 / 替换 / 目标作用域释放）
//! 都会入队一条 [`AnimationCompletion`]，由 [`dispatch_completions`] 在「产生它的那一帧内」
//! （自然完成）或「下一帧入口」（帧外终止）派发给 [`App::on_animation_complete`] 注册的回调；
//! 自然完成同时 `Dispatcher::wake` 一次，让**下一轮 `on_frame` 钩子**一定能看到终值。

#[cfg(any(test, feature = "test-support"))]
use std::time::Duration;
use std::time::Instant;

use super::{AnimationCompletion, AnimationEndKind, AnimationSlot, AnimationTarget, App};
use crate::{
    error::EngineError,
    interaction::{AnimatedValue, AnimationId, AnimationSpec, animation as anim},
    math::Point,
    node::{id::NodeId, signal::SignalId},
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

// ── E10：动画完成通知 ─────────────────────────────────────────────────────

/// 入队一条动画终止事件，并**唤醒平台泵一轮**（不请求帧：I3）。
///
/// 唤醒只保证「下一轮钩子 / 帧入口」会派发事件（`Dispatcher::wake` 是合并位）；应用回调里做的
/// 状态修改若需要重绘，由常规写路径（`App::write` 等）请求帧。自然完成路径即使没有注册回调也
/// 会走这里 —— 这正是 P2-4b R01/R02 的根治点：泵不会再停在「最后一 tick 之后、没有下一轮钩子」
/// 的空隙里。
fn queue_completion(app: &mut App, id: AnimationId, kind: AnimationEndKind) {
    app.animation_completions
        .push_back(AnimationCompletion { id, kind });
    app.dispatcher.wake();
}

/// 收集「同一目标」上会被替换的槽（`App::animate` / 零时长立即到位用）。
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

/// 派发待处理的动画终止事件（`App::run_frame_hook` / `App::frame` 入口 / tick 之后调用）。
///
/// 单槽回调的借用用「take → 循环调用 → 放回」的方式解决；回调里再取消 / 替换动画产生的事件
/// 入队后由**本循环**继续消费（`dispatching_animation_completions` 只防止重入派发，不丢事件）。
/// 没有注册回调时事件直接丢弃：完成语义仍由自然完成时的「下一轮钩子唤醒」保证可见。
pub(crate) fn dispatch_completions(app: &mut App) {
    if app.dispatching_animation_completions || app.animation_completions.is_empty() {
        return;
    }
    app.dispatching_animation_completions = true;
    if let Some(mut handler) = app.animation_complete_handler.take() {
        while let Some(event) = app.animation_completions.pop_front() {
            handler(app, event);
        }
        // 回调里重新注册了处理器（后写覆盖）⇒ 保留新的，不把旧的放回去。
        if app.animation_complete_handler.is_none() {
            app.animation_complete_handler = Some(handler);
        }
    } else {
        app.animation_completions.clear();
    }
    app.dispatching_animation_completions = false;
}

/// 作用域释放时取消引用已释放瞬态值的动画槽，并为**公开动画**入队 `Released`。
///
/// 消费者：`signal_api::release_scope`。取消发生在槽位回收之后、同一调用内，中间没有帧推进
/// （见 `release_scope` 的文档）；滚动槽是引擎内部驱动，不产生公开完成事件。
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

/// 丢弃某个窗口尚未派发的动画终止事件（关窗：窗口与节点已经不存在，应用没有可收尾的对象；
/// 未派发事件若留着会在下一轮把死窗口句柄交给应用回调）。
pub(crate) fn drop_completions_for_window(app: &mut App, window: WindowId) {
    app.animation_completions
        .retain(|event| event.id.window() != window);
}

/// 卸载子树时取消与该子树绑定的动画槽，返回取消条数（E16；`App::remove_node`）。
///
/// 判据（两条互补，避免误杀共享动画）：
/// 1. **重绘节点全部落在被卸载集合内** ⇒ 动画驱动的视觉目标已经不存在。部分节点存活的槽保留，
///    但把死节点从重绘列表里剔除（`tick_animations` 不再给陈旧句柄标脏）；
/// 2. **滚动动画的 `ScrollHandle` 节点被卸载** ⇒ 滚动槽已随树消失，继续插值只会空转请求帧。
///    滚动槽是引擎内部驱动，不产生公开完成事件。
///
/// 被取消的公开动画按 [`AnimationEndKind::Removed`] 入队完成事件（帧外终止，下一帧入口派发）：
/// 应用侧按 `AnimationId` 维护的登记表（kit `AnimationRouter` 等）因此不会留下永不完成的条目。
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
        // ② 滚动动画按 handle 归属（`nodes` 恒为空，不能用重绘节点判）。
        if let AnimationTarget::Scroll { handle, .. } = &slot.target
            && removed.contains(&handle.node())
        {
            count += 1;
            return false;
        }
        let had_nodes = !slot.nodes.is_empty();
        slot.nodes.retain(|node| !removed.contains(node));
        // ① 原本没有重绘节点的动画是窗口级驱动（不归属于某棵子树）⇒ 保留。
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
    // 同一目标重定向：旧槽必须让位（否则两条插值同时写同一个瞬态值 ⇒ 回弹/抖动）。
    // E10：被替换的旧动画报告 `Replaced`（新动画继续驱动同一个值）。
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

/// 零时长动画：**立即到位** + 取消同一目标上的旧槽（R01-13）。
///
/// 旧实现只在 `duration == 0` 分支里写一次瞬态值就 `return`，**不清旧槽**：上一次 `animate`
/// 注册的长动画仍在 `app.animations` 里，同一帧的 `tick_animations` 会继续把同一个
/// `Transient` 推向旧目标值（「设成 0 ms 还是被拉回 1.0」的回弹）。零时长的语义是
/// 「不要动画」，所以与非零路径同一规则：同一目标只能有一条驱动源。
///
/// E10：旧槽报告 `Replaced`、新动画立即报告 `Finished`（终值已经写入）。
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
        // `AnimatedValue` 没有滚动变体：滚动动画只由 `App::scroll_to` 内部注册（不经本函数）。
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

/// 取消动画（幂等）。
///
/// E10：被取消的动画报告 `Cancelled`（**不**报告 `Finished`），并唤醒泵一轮派发事件。
/// 消费者：`App::cancel_animation`。
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

/// 该窗口是否还有未完成的动画。
/// 消费者：`App::animations_running`。
#[must_use]
pub(crate) fn animations_running(app: &App, window: WindowId) -> bool {
    app.animations.iter().any(|slot| slot.window == window)
}

/// 全部窗口是否还有未完成的动画（[`needs_continuous_frames`] 的组成部分：只有动画需要按显示节拍
/// 推进；连续帧由平台泵的合成器节拍线程 `FramePacer` 提供，`SetTimer` 只保留 a11y 补发与 E10
/// 一次性截止时刻，空闲无动画且无待派发定时器时不挂表）。
/// 消费者：`App::has_animations`。
pub(crate) fn has_animations(app: &App) -> bool {
    !app.animations.is_empty()
}

/// 平台是否需要持续出帧：只有**动画**需要按显示节拍推进。
///
/// Debug HUD **不**是帧源（P-C）：它只在应用自己出帧时观察并刷新文本，静止窗口不产生帧，
/// 因此这里不能因为它可见就把平台拉进按刷新率出帧的模式（那正是「HUD 打开 ⇒ 60 Hz 空转、
/// 却只提交 2 Hz」的来源）。
/// 消费者：`App::needs_continuous_frames`。
pub(crate) fn needs_continuous_frames(app: &App) -> bool {
    app.has_animations()
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
    // E10：本轮自然完成的公开动画（滚动槽是内部驱动，不产生完成事件）。
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
                // 同上：瞬态值变了 ⇒ 读到它的 `bind_style` 必须重算，然后才谈重绘。
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
    // 终值已写入、节点已标脏 ⇒ **本帧内**派发完成事件：回调里的收尾（卸载节点 / 关浮层）
    // 参与同一帧的样式 / 布局 / 绘制。`queue_completion` 同时唤醒泵一轮，保证没有注册回调时
    // 下一轮 `on_frame` 钩子也能看到终值（R01/R02 的根治点，不请求空帧）。
    for id in finished {
        queue_completion(app, id, AnimationEndKind::Finished);
    }
    dispatch_completions(app);
}

/// 把引擎时钟推进到真实时间（平台泵每轮一次；测试用 `TestApp::advance`）。
/// 消费者：`App::sync_clock`。
pub(crate) fn sync_clock(app: &mut App) {
    app.clock = Instant::now();
}

/// 平台泵的轮首时钟同步：推进到真实时间并标记活时钟（E10c；`App::sync_clock_live`）。
///
/// `clock_live` 由 `App::new` 的生产默认置位（首次泵启动前登记也必须只晚不早，E10c-R01），
/// 测试宿主 `TestApp` 构造后关闭；平台泵每轮调用本方法刷新时钟并保持置位。一次性定时器的
/// 登记基准在活时钟下取登记瞬间真实时间，否则取引擎时钟（测试的 `TestApp::advance` 必须保持
/// deadline 可精确断言）。
pub(crate) fn sync_clock_live(app: &mut App) {
    app.clock = Instant::now();
    app.clock_live = true;
}

/// 推进测试时钟（`TestApp::advance` 的唯一实现；只在测试构建里存在）。
/// 消费者：`App::advance_clock`。
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
        math::{Rect, Size},
        node::builder::NodeBuilder,
        window::WindowOptions,
    };

    /// **AR7 M4 变异防线**：把 Debug HUD 分支加回 [`needs_continuous_frames`] 时本用例必须变红。
    ///
    /// HUD 是纯观察者（P-C）：它可见**不**等于「平台需要按刷新率出帧」—— 只有动画需要。
    #[test]
    fn the_debug_hud_never_asks_for_continuous_frames() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            size: Size::new(320.0, 240.0),
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        let node = NodeBuilder::boxed()
            .layout_override(Rect::new(0.0, 0.0, 32.0, 32.0))
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

        // 动画结束后 HUD 仍然可见 ⇒ 回到不需要连续帧（HUD 不是帧源）。
        app.advance_clock(Duration::from_millis(60));
        let _ = app.frame(window);
        assert!(!has_animations(&app), "前置条件：动画已经结束");
        assert!(
            !needs_continuous_frames(&app),
            "动画结束 + HUD 可见 ⇒ 仍然不需要连续帧"
        );
    }
}
