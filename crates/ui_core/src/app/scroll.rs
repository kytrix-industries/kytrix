//! 滚动容器：句柄、内容尺寸、偏移写入与夹紧、状态快照。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use smallvec::SmallVec;

use super::{AnimationSlot, AnimationTarget, App};
use crate::{
    error::EngineError,
    interaction::{AnimationSpec, ScrollConfig, ScrollHandle, ScrollSlot, ScrollState, scroll},
    math::{Point, Size},
    node::id::NodeId,
    paint::style::Clip,
    window::WindowId,
};

/// 已有滚动容器节点的句柄（`None` = 该节点不是滚动容器）。
/// 消费者：`App::scroll_handle`。
#[must_use]
pub(crate) fn scroll_handle(app: &App, window: WindowId, node: NodeId) -> Option<ScrollHandle> {
    app.windows
        .get(&window)?
        .tree()
        .scroll_slot(node)
        .map(|_| ScrollHandle::new(window, node))
}

// ── P10：滚动 ──────────────────────────────────────────────────────────────

/// 让一个节点成为滚动容器（要求 `Style.clip == Clip::Bounds`）。
///
/// # Errors
/// `StaleNode`（节点不存在）/ `NotScrollable`（未裁剪）。
/// 消费者：`App::make_scroll`。
pub(crate) fn make_scroll(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    config: ScrollConfig,
) -> Result<ScrollHandle, EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::NotScrollable { node });
    };
    let slot = target.tree().get(node)?;
    if slot.clip() != Clip::Bounds {
        return Err(EngineError::NotScrollable { node });
    }
    let viewport = scroll::viewport(target.tree(), node).size;
    // `line_height` 是「一行 = 多少像素」的换算系数，除零会让 `on_scroll` 的残量计算
    // 得到 NaN 并写进 offset（review P2-10）。公开字段没有别的校验点，在这里一次性拒绝。
    if !config.line_height.is_finite() || config.line_height <= 0.0 {
        return Err(EngineError::NotScrollable { node });
    }
    if let Some(slot) = target.tree_mut().ensure_interaction(node) {
        slot.scroll = Some(ScrollSlot {
            offset: Point::ZERO,
            content: viewport,
            axis: config.axis,
            line_height: config.line_height,
        });
    }
    // 反向索引：`clamp_scroll_offsets` 每帧遍历滚动容器，不再扫全树（R03-1）。
    target.tree_mut().note_scroll_container(node);
    Ok(ScrollHandle::new(window, node))
}

/// 内容尺寸（滚动范围的来源；虚拟列表由引擎写）。
///
/// # Errors
/// `NotScrollable`（不是滚动容器）。
/// 消费者：`App::set_scroll_content`。
pub(crate) fn set_scroll_content(
    app: &mut App,
    handle: &ScrollHandle,
    content: Size,
) -> Result<(), EngineError> {
    let window = handle.window();
    let node = handle.node();
    let Some(target) = app.windows.get_mut(&window) else {
        return Ok(());
    };
    let Some(slot) = target.tree_mut().scroll_slot_mut(node) else {
        return Err(EngineError::NotScrollable { node });
    };
    // 非有限尺寸会让 `max_offset` 算出 NaN，之后每帧 `next == slot.offset` 恒 false
    // ⇒ 永久请求帧（与 `line_height = 0` 同一机制；第二轮复审 P2-3）。入口处拒绝。
    if ![content.width, content.height]
        .iter()
        .all(|value| value.is_finite())
    {
        return Err(EngineError::NotScrollable { node });
    }
    slot.content = content;
    Ok(())
}

/// 程序化滚动（clamp 后写入；同值 ⇒ 完全 no-op）。
///
/// 只产生**视口矩形**的 damage：不写脏位、不跑布局、不跑样式求值。
///
/// **唯一写点 ⇒ 唯一取消点**（R02-10 / P1 回归）：写入前先取消该槽上的滚轮平滑动画。
/// 否则「滚轮动画还在跑时按 End / PageDown / 应用调用 `scroll_to`」会被下一帧的动画插值
/// 拉回原处（探针：`End` 到 300.0 后一帧被拉回 16.75）。动画自己的 tick 也经过这里
/// （`tick_animations` 已把槽取出，取消是 no-op），因此不会自我打断。
///
/// # Errors
/// `StaleNode`（节点已卸载）/ `NotScrollable`（不是滚动容器）。
/// 消费者：`App::scroll_to`。
pub(crate) fn scroll_to(
    app: &mut App,
    handle: &ScrollHandle,
    offset: Point,
) -> Result<(), EngineError> {
    // NaN/inf 偏移会在 `clamp` 后原样保留（`f32::clamp` 对 NaN 返回 NaN），随后
    // `next == slot.offset` 恒 false ⇒ 每帧自请求重绘（第二轮复审 P2-3）。入口处拒绝。
    if ![offset.x, offset.y].iter().all(|value| value.is_finite()) {
        return Err(EngineError::NotScrollable {
            node: handle.node(),
        });
    }
    let window = handle.window();
    let node = handle.node();
    // 先做只读校验（不存在/非滚动容器 ⇒ 不碰动画），再取消动画（需要 `&mut App`）。
    {
        let Some(target) = app.windows.get(&window) else {
            return Ok(());
        };
        if target.tree().get(node).is_err() {
            return Err(EngineError::StaleNode { node });
        }
        if target.tree().scroll_slot(node).is_none() {
            return Err(EngineError::NotScrollable { node });
        }
    }
    // 程序化/键盘滚动接管：残留的滚轮动画必须作废（同值写入也要取消 —— 否则「已经到底」
    // 的 End 会让动画继续把偏移拉回）。
    super::animation::cancel_scroll_animation(app, handle);
    let Some(target) = app.windows.get_mut(&window) else {
        return Ok(());
    };
    let Some(slot) = target.tree().scroll_slot(node) else {
        return Err(EngineError::NotScrollable { node });
    };
    let viewport = scroll::viewport(target.tree(), node);
    let mut probe = slot;
    probe.offset = offset;
    let next = scroll::clamp_offset(&probe, viewport.size);
    if next == slot.offset {
        return Ok(());
    }
    if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
        slot.offset = next;
    }
    // damage 是**屏幕坐标**：视口在内容坐标里，必须减掉祖先滚动偏移（容器自身偏移不算）。
    let damage = scroll::to_screen(target.tree(), node, viewport);
    target.request_damage(damage);
    // P11：a11y 的 bounds 也是屏幕化的内容坐标 ⇒ 偏移变了，整棵子树必须重发。
    // 唯一写点在 `scroll_to`（`scroll_by` / 滚轮 / 动画滚动 / a11y action 全汇到这里）。
    target.a11y_note_scroll(node);
    app.request_frame();
    app.rewindow_virtual(window, node);
    Ok(())
}

/// 布局之后把所有滚动容器的偏移重新夹紧（P2-3.3）。
///
/// 偏移只在**显式滚动**时被 `clamp_offset` 约束（`scroll_to` 是唯一写点），而可用范围
/// `content − viewport` 会随布局变化：列表变短、视口变高、窗口放大都会让它收缩。
/// 不重夹就会留下「滚到超过内容末尾」的状态 —— 表现为底部空白、或下一次滚轮只把
/// 越界量吃掉而不滚动。虚拟列表的 `rewindow` 也依赖偏移是合法的。
///
/// 只在**布局实际重跑过**之后调用（帧内 `layout_if_dirty` 之后）：没有重排时范围不会变，
/// 每帧遍历所有滚动槽是纯浪费。
/// 消费者：`App::normalize_scroll_offsets`。
pub(crate) fn normalize_scroll_offsets(app: &mut App, window: WindowId) {
    if app.clamp_scroll_offsets(window) {
        // 帧首夹紧可能发生在没有其它脏位的帧里（「内容变短」不标任何脏位），
        // 必须自己请求一帧，否则会停在 Idle 上永远不修。
        app.request_frame();
    }
}

/// 把所有滚动槽的偏移夹回 `content − viewport` 范围；返回是否有变化。
///
/// 调用点有两个：帧首（用上一帧的视口尺寸）与**布局之后**（用本帧的新视口尺寸）。
/// 只做帧首夹紧时，窗口变大导致视口变大的那一帧仍会用旧上限，于是内容上移过多 /
/// 底部空白会被画出来，而下一帧不一定存在（泵会因为无脏位而阻塞）——参见复审 P2-8。
/// 消费者：`App::clamp_scroll_offsets`。
pub(crate) fn clamp_scroll_offsets(app: &mut App, window: WindowId) -> bool {
    let Some(target) = app.windows.get_mut(&window) else {
        return false;
    };
    // 反向索引（挂载 `ScrollSlot` 时登记）：成本 O(滚动容器数)，与节点总数无关。
    let nodes: SmallVec<[crate::node::id::NodeId; 8]> =
        target.tree().scroll_containers().iter().copied().collect();
    let mut changed = false;
    for node in nodes {
        let Some(slot) = target.tree().scroll_slot(node) else {
            continue;
        };
        let viewport = scroll::viewport(target.tree(), node);
        let next = scroll::clamp_offset(&slot, viewport.size);
        if next == slot.offset {
            continue;
        }
        if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
            slot.offset = next;
        }
        // damage 是屏幕坐标（要减掉祖先滚动偏移，容器自身偏移不算）。
        let damage = scroll::to_screen(target.tree(), node, viewport);
        target.request_damage(damage);
        target.a11y_note_scroll(node);
        changed = true;
    }
    changed
}

/// 相对滚动。
///
/// # Errors
/// 同 [`App::scroll_to`]。
/// 消费者：`App::scroll_by`。
pub(crate) fn scroll_by(
    app: &mut App,
    handle: &ScrollHandle,
    delta: Point,
) -> Result<(), EngineError> {
    let current = app
        .scroll_state(handle)
        .map_or(Point::ZERO, |state| state.offset);
    app.scroll_to(handle, Point::new(current.x + delta.x, current.y + delta.y))
}

/// 滚动状态快照。
/// 消费者：`App::scroll_state`。
#[must_use]
pub(crate) fn scroll_state(app: &App, handle: &ScrollHandle) -> Option<ScrollState> {
    let target = app.windows.get(&handle.window())?;
    let node = handle.node();
    let slot = target.tree().scroll_slot(node)?;
    Some(ScrollState {
        offset: slot.offset,
        content: slot.content,
        viewport: scroll::viewport(target.tree(), node).size,
        axis: slot.axis,
    })
}

/// 滚轮 → 平滑滚动（登记/重定向一个 `Scroll` 动画槽）。
/// 消费者：`App::scroll_to_animated`。
pub(crate) fn scroll_to_animated(
    app: &mut App,
    handle: &ScrollHandle,
    target: Point,
) -> Result<(), EngineError> {
    let Some(state) = app.scroll_state(handle) else {
        return Err(EngineError::NotScrollable {
            node: handle.node(),
        });
    };
    let spec = AnimationSpec::short();
    let window = handle.window();
    // 已有同 handle 的滚动动画 ⇒ 从当前偏移重新起步。
    app.animations.retain(|slot| {
        !matches!(&slot.target, AnimationTarget::Scroll { handle: existing, .. } if *existing == *handle)
    });
    let id = app.next_animation;
    app.next_animation = app.next_animation.wrapping_add(1);
    app.animations.push(AnimationSlot {
        id,
        window,
        target: AnimationTarget::Scroll {
            handle: *handle,
            from: state.offset,
            to: target,
        },
        start: app.clock,
        duration: spec.duration,
        easing: spec.easing,
        nodes: Vec::new(),
    });
    app.request_frame();
    Ok(())
}
