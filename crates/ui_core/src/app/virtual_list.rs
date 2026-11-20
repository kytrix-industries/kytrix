//! 虚拟列表：容器、条目窗口重建、状态与行下标查询，以及 E3 的逐项行高。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。
//!
//! 两套行布局（§11.3 / E3）：
//! - 单一 `extent`：行 `i` 的 top = `i * extent`、内容高度 = `count * extent`（v1 语义，逐字节保留）；
//! - 逐项高度：前缀和（`VirtualSlot::set_extents` 里 O(n) 重建）+ 可见区间二分（滚动帧 O(log n)）。
//!
//! 行高**只由应用给**（`set_virtual_extents` / `VirtualListConfig.extents`）：引擎不自动测量，
//! 回写仍复用同一棵 taffy 树的 `NodeTree::set_virtual_row`（I5：不引入第二份布局真相）。

use super::App;
use crate::{
    error::EngineError,
    interaction::{
        ItemAlign, ScrollAxis, ScrollConfig, ScrollHandle, ScrollSlot, VirtualList,
        VirtualListConfig, VirtualListState, VirtualSlot, scroll, virtual_list,
    },
    math::{Point, Size},
    node::{builder::NodeBuilder, dependency::Owner, id::NodeId, tree::NodeTree},
    paint::style::Clip,
    window::WindowId,
};

// ── P10：虚拟列表 ──────────────────────────────────────────────────────────

/// 让一个滚动容器变成虚拟列表（要求 `Clip::Bounds` + `position: relative`）。
///
/// `config.extents` 是**预置**的逐项高度：先记住，`set_virtual_items` 的条目数与之匹配时生效
/// （创建时就已知行高的常见路径）；不匹配则丢弃，应用随后用
/// [`App::set_virtual_extents`] 整体替换。
///
/// # Errors
/// `StaleNode` / `NotVirtualList` / `InvalidVirtualExtent`（预置行高非有限、为负或累计溢出）。
/// 消费者：`App::make_virtual_list`。
pub(crate) fn make_virtual_list(
    app: &mut App,
    window: WindowId,
    container: NodeId,
    config: VirtualListConfig,
) -> Result<VirtualList, EngineError> {
    let Some(target) = app.windows.get_mut(&window) else {
        return Err(EngineError::NotVirtualList { node: container });
    };
    let node = target.tree().get(container)?;
    if node.clip() != Clip::Bounds
        || node.style().inner.position != taffy::style::Position::Relative
    {
        return Err(EngineError::NotVirtualList { node: container });
    }
    if let Some(extents) = config.extents.as_deref() {
        validate_extents(extents)?;
    }
    let viewport = scroll::viewport(target.tree(), container).size;
    let Some(slot) = target.tree_mut().ensure_interaction(container) else {
        return Err(EngineError::StaleNode { node: container });
    };
    slot.scroll = Some(ScrollSlot {
        offset: Point::ZERO,
        content: viewport,
        axis: ScrollAxis::Vertical,
        line_height: ScrollConfig::default().line_height,
    });
    let mut virtual_slot = VirtualSlot::new(config.extent.max(1.0), config.overscan);
    if let Some(extents) = config.extents {
        virtual_slot.set_extents(extents);
    }
    slot.virtual_list = Some(virtual_slot);
    // 反向索引：`App::rewindow_virtuals` 每帧只需要遍历虚拟列表容器，不再扫全树（R03-1）。
    target.tree_mut().note_virtual_container(container);
    Ok(VirtualList::new(window, container))
}

/// 设置条目总数与行工厂，并重建可见窗口（只在**新建行**时调用 `build`）。
///
/// 条目数变化时若旧逐项高度长度不符：先把旧锚点（旧 prefix 的 first + 行内偏移）按新条目数
/// clamp 并回落到单一 `extent`，再丢弃旧高度（§11.3：count 由本方法决定、不在
/// `set_virtual_extents` 里改变；应用随后应调用 `set_virtual_extents` 提供新高度）。
/// 回落时**已挂载行同步按 `extent` 重排**，不会把旧逐项布局的 top/height 留到中间帧。
///
/// # Errors
/// `NotVirtualList`（未 `make_virtual_list`）。
/// 消费者：`App::set_virtual_items`。
pub(crate) fn set_virtual_items(
    app: &mut App,
    list: &VirtualList,
    count: usize,
    build: impl FnMut(usize) -> NodeBuilder + 'static,
) -> Result<(), EngineError> {
    let window = list.window();
    let node = list.node();
    // 只读快照：旧条目数 / 当前偏移 / 旧锚点 / 视口宽度（锚点补偿只用得到前三个）。
    let Some(target) = app.windows.get(&window) else {
        return Ok(());
    };
    let Some(slot) = target.tree().virtual_slot(node) else {
        return Err(EngineError::NotVirtualList { node });
    };
    let old_count = slot.count;
    let Some(scroll_slot) = target.tree().scroll_slot(node) else {
        return Err(EngineError::NotVirtualList { node });
    };
    let offset = scroll_slot.offset.y;
    let viewport_width = scroll::viewport(target.tree(), node).size.width;
    let anchor = virtual_list::offset_anchor(slot, offset);
    // 写侧：条目数 / 行工厂 / a11y 集合总数；旧逐项高度失效时锚点补偿。
    let (anchored, extents_dropped) = {
        let Some(target) = app.windows.get_mut(&window) else {
            return Ok(());
        };
        let Some(slot) = target.tree_mut().virtual_slot_mut(node) else {
            return Err(EngineError::NotVirtualList { node });
        };
        let mut anchored = None;
        let mut extents_dropped = false;
        if slot
            .extents
            .as_deref()
            .is_some_and(|extents| extents.len() != count)
        {
            slot.clear_extents();
            slot.count = count;
            extents_dropped = true;
            if count != old_count {
                anchored = Some(virtual_list::offset_for_anchor(slot, anchor.0, anchor.1));
            }
        } else {
            slot.count = count;
        }
        slot.builder = Some(Box::new(build));
        let content = virtual_list::content_height(slot);
        target.tree_mut().mark_a11y(node);
        if anchored.is_some()
            && let Some(scroll) = target.tree_mut().scroll_slot_mut(node)
        {
            scroll.content = Size::new(viewport_width, content);
        }
        (anchored, extents_dropped)
    };
    // 锚点补偿走唯一偏移写点（clamp / damage / a11y / 取消滚轮动画都在 `scroll_to` 里）；
    // 与重建窗口同批 ⇒ 条目数变化最多唤醒一帧（I3）。
    app.batch(|app| {
        if let Some(target_offset) = anchored
            && target_offset != offset
        {
            app.scroll_to(
                &ScrollHandle::new(window, node),
                Point::new(0.0, target_offset),
            )?;
        }
        app.rewindow_virtual(window, node);
        // 逐项高度被丢弃 ⇒ 已挂载行必须按单一 `extent` 同步重排：只重建可见窗口会留下
        // 旧逐项布局的 top/height，中间帧把可见行画到视口外（列表空白，F2）。
        if extents_dropped {
            reposition_mounted_rows(app, window, node);
        }
        Ok(())
    })
}

/// 虚拟列表状态快照。
/// 消费者：`App::virtual_list_state`。
#[must_use]
pub(crate) fn virtual_list_state(app: &App, list: &VirtualList) -> Option<VirtualListState> {
    let target = app.windows.get(&list.window())?;
    let node = list.node();
    let slot = target.tree().virtual_slot(node)?;
    let scroll = target.tree().scroll_slot(node)?;
    let viewport = scroll::viewport(target.tree(), node).size;
    let range = virtual_list::visible_window(slot, scroll.offset.y, viewport.height);
    Some(VirtualListState {
        count: slot.count,
        first: range.start,
        last: range.end,
        mounted: slot.mounted.len(),
        offset: scroll.offset,
        viewport,
    })
}

/// 行节点 → 条目下标（a11y 的 `pos_in_set`）。
/// 消费者：`App::virtual_row_index`。
#[must_use]
pub(crate) fn virtual_row_index(app: &App, list: &VirtualList, row: NodeId) -> Option<usize> {
    let target = app.windows.get(&list.window())?;
    let slot = target.tree().virtual_slot(list.node())?;
    slot.mounted
        .iter()
        .find(|(_, mounted)| *mounted == row)
        .map(|(index, _)| *index)
}

/// 布局后/视口变化后修复所有虚拟列表的可见窗口（幂等：区间不变则零工作）。
/// 消费者：`App::rewindow_virtuals`。
pub(crate) fn rewindow_virtuals(app: &mut App, window: WindowId) {
    // 反向索引（挂载时登记）：成本 O(虚拟列表数)，与节点总数无关。
    let containers: Vec<NodeId> = app.windows.get(&window).map_or_else(Vec::new, |target| {
        target.tree().virtual_containers().to_vec()
    });
    for node in containers {
        app.rewindow_virtual(window, node);
    }
}

/// 按当前偏移重建可见窗口（挂载缺的、回收多的）。
///
/// 逐项高度只在**内容/偏移**变化时改变行几何；滚动帧本身只做前缀和二分（O(log n)），
/// 不重建前缀和（I3 的预算前提）。
/// 消费者：`App::rewindow_virtual`。
pub(crate) fn rewindow_virtual(app: &mut App, window: WindowId, node: NodeId) {
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    // 只读：可见区间 / 内容高度 / 视口（变高走 `prefix` 二分）。
    let Some((range, content, viewport)) = target.tree().virtual_slot(node).and_then(|slot| {
        let scroll = target.tree().scroll_slot(node)?;
        let viewport = scroll::viewport(target.tree(), node).size;
        Some((
            virtual_list::visible_window(slot, scroll.offset.y, viewport.height),
            virtual_list::content_height(slot),
            viewport,
        ))
    }) else {
        return;
    };
    // 缺的行先算好几何（写侧要 `tree_mut`，不能再借 `VirtualSlot`）。
    let mut missing: Vec<(usize, f32, f32)> = Vec::new();
    if let Some(slot) = target.tree().virtual_slot(node) {
        for index in range.clone() {
            if slot.mounted.iter().any(|(mounted, _)| *mounted == index) {
                continue;
            }
            let (top, height) = virtual_list::row_top_height(slot, index);
            missing.push((index, top, height));
        }
    }
    // 回收
    let stale: Vec<(usize, NodeId)> = target
        .tree()
        .virtual_slot(node)
        .map(|slot| {
            slot.mounted
                .iter()
                .copied()
                .filter(|(index, _)| !range.contains(index))
                .collect()
        })
        .unwrap_or_default();
    let mut changed = false;
    for (_, row) in &stale {
        // 回收行同样必须解开依赖边（P1-2.5）：滚动是最高频操作，留下的死边会线性放大
        // 每一次 `App::write`。
        if let Ok(detached) = target.tree_mut().remove(*row) {
            for (removed, deps) in detached {
                for signal in deps {
                    app.graph.detach(
                        signal,
                        Owner::Node {
                            window,
                            node: removed,
                        },
                    );
                }
            }
            changed = true;
        }
    }
    if let Some(slot) = target.tree_mut().virtual_slot_mut(node) {
        slot.mounted.retain(|(index, _)| range.contains(index));
    }
    // 挂载
    for (index, top, height) in missing {
        let Some(mut builder) = target
            .tree_mut()
            .virtual_slot_mut(node)
            .and_then(|slot| slot.builder.take())
        else {
            break;
        };
        let built = builder(index).build(target.tree_mut(), node);
        if let Some(slot) = target.tree_mut().virtual_slot_mut(node) {
            slot.builder = Some(builder);
        }
        let Ok(row) = built else {
            break;
        };
        target.tree_mut().set_virtual_row(row, top, height);
        if let Some(slot) = target.tree_mut().virtual_slot_mut(node) {
            slot.mounted.push((index, row));
        }
        changed = true;
    }
    // 内容尺寸 + 结构脏位
    if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
        slot.content = Size::new(viewport.width, content);
    }
    if changed {
        target.tree_mut().mark_structure(node);
        app.request_frame();
    }
}

// ── E3：逐项行高（§11.3） ─────────────────────────────────────────────────

/// 逐项高度**整体替换**（`extents.len()` 必须等于当前条目数），引擎重算前缀和
/// （O(n)）并按锚点补偿滚动偏移：替换前记「首个可见 index `f` + 行内偏移 `d = offset −
/// prefix[f]`」，替换后 `offset = prefix'[f] + d`（clamp 到新内容范围）。
///
/// **同一次调用内完成**：锚点补偿不额外请求帧（I3）——有可见变化时整个调用最多唤醒一次，
/// 与旧高度完全相同时直接 no-op（不写状态、不请求帧）。
///
/// # Errors
/// `NotVirtualList`（list 失效 / 未 `make_virtual_list`）、`VirtualListExtents`
/// （长度与条目数不符）、`InvalidVirtualExtent`（逐项高度非有限、为负或累计溢出）。
/// 消费者：`App::set_virtual_extents`。
pub(crate) fn set_virtual_extents(
    app: &mut App,
    list: &VirtualList,
    extents: &[f32],
) -> Result<(), EngineError> {
    let window = list.window();
    let node = list.node();
    let Some(target) = app.windows.get(&window) else {
        return Err(EngineError::NotVirtualList { node });
    };
    let Some(slot) = target.tree().virtual_slot(node) else {
        return Err(EngineError::NotVirtualList { node });
    };
    if extents.len() != slot.count {
        return Err(EngineError::VirtualListExtents {
            expected: slot.count,
            actual: extents.len(),
        });
    }
    validate_extents(extents)?;
    // 整体替换为同值：没有状态变化 ⇒ 不请求帧（I3）。
    if slot.extents.as_deref() == Some(extents) {
        return Ok(());
    }
    app.batch(|app| apply_virtual_extents(app, window, node, extents))
}

/// `set_virtual_extents` 的批处理体（`batch` 把内部多次 `request_frame` 合并成一次唤醒）。
fn apply_virtual_extents(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    extents: &[f32],
) -> Result<(), EngineError> {
    // 只读：旧锚点（旧 prefix 的 f + d）、当前偏移、视口。
    let Some((anchor, current_offset, viewport)) = app.windows.get(&window).and_then(|target| {
        let slot = target.tree().virtual_slot(node)?;
        let scroll = target.tree().scroll_slot(node)?;
        Some((
            virtual_list::offset_anchor(slot, scroll.offset.y),
            scroll.offset.y,
            scroll::viewport(target.tree(), node).size,
        ))
    }) else {
        return Err(EngineError::NotVirtualList { node });
    };
    // 写：新逐项高度（O(n) 前缀和）、内容高度、按新布局重算的锚点偏移（已夹紧）。
    let (target_offset, offset_changed) = {
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(EngineError::NotVirtualList { node });
        };
        let Some(slot) = target.tree_mut().virtual_slot_mut(node) else {
            return Err(EngineError::NotVirtualList { node });
        };
        slot.set_extents(extents.to_vec().into_boxed_slice());
        let content = virtual_list::content_height(slot);
        let max = (content - viewport.height).max(0.0);
        let target_offset = scroll::clamp(
            virtual_list::offset_for_anchor(slot, anchor.0, anchor.1),
            max,
        );
        if let Some(scroll) = target.tree_mut().scroll_slot_mut(node) {
            scroll.content = Size::new(viewport.width, content);
        }
        (target_offset, target_offset != current_offset)
    };
    // 锚点补偿走唯一偏移写点（clamp / 视口 damage / a11y 子树重发 / 取消滚轮动画）。
    if offset_changed {
        app.scroll_to(
            &ScrollHandle::new(window, node),
            Point::new(0.0, target_offset),
        )?;
    }
    app.rewindow_virtual(window, node);
    // 受影响行：top/height 与新前缀不一致的已挂载行标 `LAYOUT|PAINT`（同一棵 taffy 树）。
    reposition_mounted_rows(app, window, node);
    Ok(())
}

/// 已挂载行按当前生效行布局重排（只对几何真的变化的行调 `set_virtual_row`）。
///
/// 两处调用：`set_virtual_extents`（新 prefix）与 `set_virtual_items` 丢弃逐项高度后的回落。
fn reposition_mounted_rows(app: &mut App, window: WindowId, node: NodeId) {
    let rows: Vec<(NodeId, f32, f32)> = {
        let Some(target) = app.windows.get(&window) else {
            return;
        };
        let Some(slot) = target.tree().virtual_slot(node) else {
            return;
        };
        slot.mounted
            .iter()
            .map(|&(index, row)| {
                let (top, height) = virtual_list::row_top_height(slot, index);
                (row, top, height)
            })
            .collect()
    };
    let mut moved = false;
    if let Some(target) = app.windows.get_mut(&window) {
        for (row, top, height) in rows {
            if row_matches(target.tree(), row, top, height) {
                continue;
            }
            target.tree_mut().set_virtual_row(row, top, height);
            moved = true;
        }
    }
    if moved {
        app.request_frame();
    }
}

/// 把第 `index` 项按 `align` 对齐到视口；`Nearest` = 已完全可见则不滚。
///
/// 落点先按「内容高度 − 视口高度」夹紧，再走唯一偏移写点 [`App::scroll_to`]。
///
/// # Errors
/// `NotVirtualList`（list 失效）、`VirtualListExtents`（`index` 越界）。
/// 消费者：`App::scroll_to_item`。
pub(crate) fn scroll_to_item(
    app: &mut App,
    list: &VirtualList,
    index: usize,
    align: ItemAlign,
) -> Result<(), EngineError> {
    let window = list.window();
    let node = list.node();
    let Some(target) = app.windows.get(&window) else {
        return Err(EngineError::NotVirtualList { node });
    };
    let Some(slot) = target.tree().virtual_slot(node) else {
        return Err(EngineError::NotVirtualList { node });
    };
    if index >= slot.count {
        return Err(EngineError::VirtualListExtents {
            expected: slot.count,
            actual: index + 1,
        });
    }
    let Some(scroll) = target.tree().scroll_slot(node) else {
        return Err(EngineError::NotVirtualList { node });
    };
    let viewport = scroll::viewport(target.tree(), node).size;
    let current = scroll.offset.y;
    let (top, height) = virtual_list::row_top_height(slot, index);
    let content = virtual_list::content_height(slot);
    let proposed = match align {
        ItemAlign::Start => top,
        ItemAlign::Center => top + height / 2.0 - viewport.height / 2.0,
        ItemAlign::End => top + height - viewport.height,
        ItemAlign::Nearest => {
            if top >= current && top + height <= current + viewport.height {
                current
            } else if top < current {
                top
            } else {
                top + height - viewport.height
            }
        }
    };
    let max = (content - viewport.height).max(0.0);
    let target_offset = scroll::clamp(proposed, max);
    if target_offset == current {
        return Ok(());
    }
    app.batch(|app| {
        app.scroll_to(
            &ScrollHandle::new(window, node),
            Point::new(0.0, target_offset),
        )
    })
}

/// 逐项高度入口校验：每项必须有限且非负（`0` 合法：折叠行），**累计高度也必须有限**
/// （否则 `prefix[count]` 溢出到 `inf`：内容高度 / clamp / `scroll_to_item` 全部失真）。
///
/// 在任何状态写入前调用（`make_virtual_list` 预置高度、`set_virtual_extents`），
/// 因此错误路径不改动 `extents` / `content` / `offset`。累计溢出的 `index` 指向
/// **使累计值首次变为非有限**的那一项。
fn validate_extents(extents: &[f32]) -> Result<(), EngineError> {
    let mut total = 0.0_f32;
    for (index, value) in extents.iter().copied().enumerate() {
        if !value.is_finite() || value < 0.0 {
            return Err(EngineError::InvalidVirtualExtent { index, value });
        }
        total += value;
        if !total.is_finite() {
            return Err(EngineError::InvalidVirtualExtent { index, value });
        }
    }
    Ok(())
}

/// 已挂载行的布局输入是否已经是 `(top, height)`（等值 ⇒ 不重复标脏）。
fn row_matches(tree: &NodeTree, row: NodeId, top: f32, height: f32) -> bool {
    tree.get(row).is_ok_and(|node| {
        let style = &node.style().inner;
        style.position == taffy::style::Position::Absolute
            && style.inset.top == taffy::style::LengthPercentageAuto::length(top)
            && style.size.height == taffy::style::Dimension::length(height)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        math::{Rect, Scale},
        paint::style::Style,
        window::WindowOptions,
    };

    type Fixture = (tokio::runtime::Runtime, App, WindowId, NodeId, VirtualList);

    fn frame(app: &mut App, window: WindowId) {
        let _ = app.frame(window).expect("window exists").expect("frame");
    }

    fn app_with_container() -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: "e3".into(),
            size: Size::new(400.0, 300.0),
            scale: Scale::ONE,
            ..WindowOptions::default()
        });
        let mut style = Style::default();
        style.inner.size = taffy::geometry::Size {
            width: taffy::style::Dimension::length(200.0),
            height: taffy::style::Dimension::length(100.0),
        };
        style.inner.position = taffy::style::Position::Relative;
        style.clip = Clip::Bounds;
        let tree = app.window_mut(window).expect("window").tree_mut();
        let root = tree.root();
        let container = NodeBuilder::boxed()
            .style(style)
            .build(tree, root)
            .expect("mount");
        frame(&mut app, window);
        (runtime, app, window, container)
    }

    fn fixture(extent: f32, overscan: usize, extents: Option<Vec<f32>>) -> Fixture {
        let (runtime, mut app, window, container) = app_with_container();
        let list = app
            .make_virtual_list(
                window,
                container,
                VirtualListConfig {
                    extent,
                    extents: extents.map(Vec::into_boxed_slice),
                    overscan,
                },
            )
            .expect("virtual list");
        (runtime, app, window, container, list)
    }

    fn set_items(app: &mut App, list: &VirtualList, count: usize) {
        app.set_virtual_items(list, count, |index| {
            NodeBuilder::boxed().id(format!("row{index}"))
        })
        .expect("items");
    }

    fn scroll_offset(app: &App, window: WindowId, container: NodeId) -> f32 {
        app.scroll_state(&app.scroll_handle(window, container).expect("scroll handle"))
            .expect("scroll state")
            .offset
            .y
    }

    fn content_height(app: &App, window: WindowId, container: NodeId) -> f32 {
        app.scroll_state(&app.scroll_handle(window, container).expect("scroll handle"))
            .expect("scroll state")
            .content
            .height
    }

    fn mounted_rows(app: &App, window: WindowId, list: &VirtualList) -> Vec<(usize, NodeId)> {
        app.window(window)
            .expect("window")
            .tree()
            .virtual_slot(list.node())
            .expect("virtual slot")
            .mounted
            .clone()
    }

    fn row_bounds(app: &App, window: WindowId, container: NodeId, row: NodeId) -> Rect {
        let tree = app.window(window).expect("window").tree();
        let origin = tree
            .get(container)
            .expect("container")
            .layout()
            .bounds
            .origin;
        let bounds = tree.get(row).expect("row").layout().bounds;
        Rect::new(
            bounds.origin.x - origin.x,
            bounds.origin.y - origin.y,
            bounds.size.width,
            bounds.size.height,
        )
    }

    /// L1：`extents` → prefix → `set_virtual_row(top, height)`；内容高度 = `prefix[count]`。
    #[test]
    fn variable_extents_position_rows_by_the_prefix() {
        let (_rt, mut app, window, container, list) = fixture(100.0, 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);

        let mut extents = vec![100.0_f32; 100];
        extents[..4].copy_from_slice(&[30.0, 40.0, 50.0, 60.0]);
        app.set_virtual_extents(&list, &extents).expect("extents");
        frame(&mut app, window);

        let prefix = [0.0_f32, 30.0, 70.0, 120.0];
        let mounted = mounted_rows(&app, window, &list);
        assert_eq!(
            mounted.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "视口 100：30 + 40 + 50 铺满"
        );
        for (index, row) in mounted {
            let bounds = row_bounds(&app, window, container, row);
            assert_eq!(bounds.origin.y, prefix[index], "row {index} top");
            assert_eq!(bounds.size.height, extents[index], "row {index} height");
        }
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!((state.first, state.last), (0, 3));
        assert_eq!(
            content_height(&app, window, container),
            extents.iter().sum::<f32>()
        );

        // 变高列表的 `scroll_to_item` 落点也走 prefix（top=120、height=60、viewport=100）。
        app.scroll_to_item(&list, 3, ItemAlign::Start)
            .expect("start");
        assert_eq!(scroll_offset(&app, window, container), 120.0);
        app.scroll_to_item(&list, 3, ItemAlign::End).expect("end");
        assert_eq!(scroll_offset(&app, window, container), 80.0);
        app.scroll_to_item(&list, 3, ItemAlign::Center)
            .expect("center");
        assert_eq!(scroll_offset(&app, window, container), 100.0);
    }

    /// 锚点规则：替换前后「同一 first index + 同一行内偏移」，且一次替换只唤醒一帧（I3）。
    #[test]
    fn replacing_extents_preserves_the_visible_anchor() {
        let (_rt, mut app, window, container, list) = fixture(100.0, 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);
        let handle = app.scroll_handle(window, container).expect("handle");
        app.scroll_to(&handle, Point::new(0.0, 150.0))
            .expect("scroll");
        frame(&mut app, window);
        let before = app.virtual_list_state(&list).expect("state");
        assert_eq!(before.first, 1);
        let inner_before = before.offset.y - 100.0;
        assert_eq!(inner_before, 50.0);

        let mut extents = vec![100.0_f32; 100];
        extents[0] = 200.0;
        let requests = app.frame_requests();
        app.set_virtual_extents(&list, &extents).expect("extents");
        assert_eq!(
            app.frame_requests() - requests,
            1,
            "一次 extents 替换 ⇒ 恰好一次唤醒（I3）"
        );
        frame(&mut app, window);

        let after = app.virtual_list_state(&list).expect("state");
        assert_eq!(after.offset.y, 250.0, "prefix'[1] + 50");
        assert_eq!(after.first, 1, "首个可见 index 不变");
        assert_eq!(after.offset.y - 200.0, inner_before, "行内偏移不变");
        let prefix = [0.0_f32, 200.0, 300.0, 400.0];
        let mounted = mounted_rows(&app, window, &list);
        assert_eq!(
            mounted.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![1, 2],
            "250..350 覆盖第 1、2 行"
        );
        for (index, row) in mounted {
            assert_eq!(
                row_bounds(&app, window, container, row).origin.y,
                prefix[index],
                "row {index} 已按新 prefix 重排"
            );
        }
    }

    /// L2（§11.3）：一次 extents 替换只产生一帧，且 damage 不退化为整帧。
    #[cfg(feature = "test-support")]
    #[test]
    fn replacing_extents_paints_one_partial_frame() {
        let (_rt, mut app, window, _container, list) = fixture(25.0, 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);
        let mut extents = vec![25.0_f32; 100];
        extents[0] = 50.0;
        app.set_virtual_extents(&list, &extents).expect("extents");
        frame(&mut app, window);
        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("extents 替换必须提交一帧");
        assert!(record.repainted, "必要条件必须重绘");
        assert!(!record.damage_full, "只重画可见行/视口，不退化为整帧");
        // 下一帧空闲 ⇒ 替换只唤醒一帧（I3）。
        let outcome = app.frame(window).expect("window").expect("frame");
        assert_eq!(outcome, crate::window::FrameOutcome::Idle);
    }

    /// 四种对齐的落点（等宽 20、视口 100、内容 2000）。
    #[test]
    fn scroll_to_item_follows_the_four_alignments() {
        let (_rt, mut app, window, container, list) = fixture(20.0, 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);

        app.scroll_to_item(&list, 10, ItemAlign::Start)
            .expect("start");
        assert_eq!(scroll_offset(&app, window, container), 200.0);
        app.scroll_to_item(&list, 10, ItemAlign::Center)
            .expect("center");
        assert_eq!(scroll_offset(&app, window, container), 160.0);
        app.scroll_to_item(&list, 10, ItemAlign::End).expect("end");
        assert_eq!(scroll_offset(&app, window, container), 120.0);

        // Nearest：行 10（200..220）已完全在 120..220 内 ⇒ 不滚、也不请求帧。
        let requests = app.frame_requests();
        app.scroll_to_item(&list, 10, ItemAlign::Nearest)
            .expect("nearest");
        assert_eq!(scroll_offset(&app, window, container), 120.0);
        assert_eq!(app.frame_requests(), requests, "已可见 ⇒ 无帧");
        // 在视口上方 / 下方 ⇒ 滚到最近的边界。
        app.scroll_to(
            &app.scroll_handle(window, container).expect("handle"),
            Point::new(0.0, 130.0),
        )
        .expect("scroll");
        app.scroll_to_item(&list, 0, ItemAlign::Nearest)
            .expect("nearest up");
        assert_eq!(scroll_offset(&app, window, container), 0.0);
        app.scroll_to_item(&list, 99, ItemAlign::Nearest)
            .expect("nearest down");
        assert_eq!(scroll_offset(&app, window, container), 1_900.0);
        // 尾部 clamp：End 也不会越过最大偏移。
        app.scroll_to_item(&list, 99, ItemAlign::End).expect("end");
        assert_eq!(scroll_offset(&app, window, container), 1_900.0);
    }

    /// I3：与旧几何等价的 extents 替换不请求帧；完全相同的第二次是纯 no-op。
    #[test]
    fn an_identical_extents_replacement_never_requests_a_frame() {
        let (_rt, mut app, window, container, list) = fixture(25.0, 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);
        let extents = vec![25.0_f32; 100];
        let requests = app.frame_requests();
        app.set_virtual_extents(&list, &extents).expect("extents");
        assert_eq!(
            app.frame_requests(),
            requests,
            "与单一 extent 等价 ⇒ 可见内容未变"
        );
        app.set_virtual_extents(&list, &extents).expect("extents");
        assert_eq!(app.frame_requests(), requests, "同值替换是 no-op");
        assert_eq!(content_height(&app, window, container), 2_500.0);
    }

    /// I3：只改屏外行 ⇒ 不请求帧，但内容高度（滚动范围）立刻更新。
    #[test]
    fn offscreen_extents_changes_do_not_force_a_frame() {
        let (_rt, mut app, window, container, list) = fixture(25.0, 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);
        let mut extents = vec![25.0_f32; 100];
        extents[99] = 100.0;
        let requests = app.frame_requests();
        app.set_virtual_extents(&list, &extents).expect("extents");
        assert_eq!(app.frame_requests(), requests, "可见行未移动 ⇒ 不请求帧");
        assert_eq!(content_height(&app, window, container), 2_575.0);
    }

    /// 强类型错误：长度不符 / 非有限 / 负值 / 越界 / 失效 list；零高与空列表合法。
    #[test]
    fn extents_and_item_index_errors_are_typed() {
        let (_rt, mut app, window, container, list) = fixture(25.0, 0, None);
        set_items(&mut app, &list, 10);
        frame(&mut app, window);

        match app.set_virtual_extents(&list, &[25.0; 9]) {
            Err(EngineError::VirtualListExtents {
                expected: 10,
                actual: 9,
            }) => {}
            other => panic!("长度不符必须是 VirtualListExtents：{other:?}"),
        }
        match app.set_virtual_extents(&list, &[]) {
            Err(EngineError::VirtualListExtents {
                expected: 10,
                actual: 0,
            }) => {}
            other => panic!("空 extents 但 count > 0 必须是 VirtualListExtents：{other:?}"),
        }
        let mut nan = vec![25.0_f32; 10];
        nan[3] = f32::NAN;
        match app.set_virtual_extents(&list, &nan) {
            Err(EngineError::InvalidVirtualExtent { index: 3, value }) => {
                assert!(value.is_nan());
            }
            other => panic!("NaN 必须是 InvalidVirtualExtent：{other:?}"),
        }
        let mut negative = vec![25.0_f32; 10];
        negative[0] = -1.0;
        match app.set_virtual_extents(&list, &negative) {
            Err(EngineError::InvalidVirtualExtent { index: 0, value }) => {
                assert_eq!(value, -1.0);
            }
            other => panic!("负值必须是 InvalidVirtualExtent：{other:?}"),
        }
        match app.set_virtual_extents(&list, &[f32::INFINITY; 10]) {
            Err(EngineError::InvalidVirtualExtent { index: 0, .. }) => {}
            other => panic!("非有限值必须是 InvalidVirtualExtent：{other:?}"),
        }
        // 越界：expected = count、actual = 1-based 位置。
        match app.scroll_to_item(&list, 10, ItemAlign::Start) {
            Err(EngineError::VirtualListExtents {
                expected: 10,
                actual: 11,
            }) => {}
            other => panic!("index 越界必须是 VirtualListExtents：{other:?}"),
        }
        // 零高合法（折叠行）：内容高度为 0、无可见行。
        app.set_virtual_extents(&list, &[0.0; 10])
            .expect("zero heights");
        frame(&mut app, window);
        assert_eq!(content_height(&app, window, container), 0.0);
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!((state.first, state.last, state.mounted), (0, 0, 0));

        // 失效 list：容器已卸载 ⇒ NotVirtualList。
        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .remove(container)
            .expect("remove");
        match app.set_virtual_extents(&list, &[25.0; 10]) {
            Err(EngineError::NotVirtualList { node }) => assert_eq!(node, container),
            other => panic!("失效 list 必须是 NotVirtualList：{other:?}"),
        }
        match app.scroll_to_item(&list, 0, ItemAlign::Start) {
            Err(EngineError::NotVirtualList { node }) => assert_eq!(node, container),
            other => panic!("失效 list 必须是 NotVirtualList：{other:?}"),
        }
    }

    /// F1 回归：全零高度（应用尚未测量）→ 真实高度时视图必须停在顶部（锚点 (0, 0)）。
    #[test]
    fn zero_heights_then_real_heights_keep_the_visible_anchor_at_the_top() {
        let (_rt, mut app, window, container, list) = fixture(100.0, 0, None);
        set_items(&mut app, &list, 10);
        frame(&mut app, window);
        app.set_virtual_extents(&list, &[0.0; 10])
            .expect("zero heights");
        frame(&mut app, window);
        assert_eq!(content_height(&app, window, container), 0.0);
        let folded = app.virtual_list_state(&list).expect("state");
        assert_eq!(
            (folded.offset.y, folded.first, folded.mounted),
            (0.0, 0, 0),
            "零内容：无可见行、偏移 0"
        );

        // 首次拿到真实行高：零内容锚点固定 (0, 0)，偏移保持 0（修复前会跳到底部 900）。
        app.set_virtual_extents(&list, &[100.0; 10])
            .expect("measured heights");
        frame(&mut app, window);
        let measured = app.virtual_list_state(&list).expect("state");
        assert_eq!(
            (measured.offset.y, measured.first),
            (0.0, 0),
            "零高 → 真实高度不得跳底"
        );
        assert_eq!(content_height(&app, window, container), 1_000.0);
    }

    /// F3 回归：单值有限但累计溢出 ⇒ `InvalidVirtualExtent`，且在任何状态写入前失败。
    #[test]
    fn overflowing_extent_totals_are_rejected_before_any_state_change() {
        let (_rt, mut app, window, container, list) = fixture(25.0, 0, None);
        set_items(&mut app, &list, 3);
        frame(&mut app, window);
        let before = app.virtual_list_state(&list).expect("state");
        let requests = app.frame_requests();
        // 3e38 单项有限、两项累计到 inf（修复前 Ok，content=inf 且随后 scroll_to_item 假报
        // NotScrollable）。
        match app.set_virtual_extents(&list, &[3e38, 3e38, 1.0]) {
            Err(EngineError::InvalidVirtualExtent { index: 1, value }) => {
                assert_eq!(value, 3e38);
            }
            other => panic!("累计溢出必须是 InvalidVirtualExtent：{other:?}"),
        }
        assert_eq!(
            content_height(&app, window, container),
            75.0,
            "错误路径不得半写 content"
        );
        assert_eq!(
            app.virtual_list_state(&list).expect("state"),
            before,
            "错误路径不改状态（先校验、后写入）"
        );
        assert_eq!(app.frame_requests(), requests, "错误路径不请求帧");
        // 同量级但累计有限 ⇒ 校验通过。
        validate_extents(&[3e38, 1.0, 1.0]).expect("finite total");

        // 预置高度（创建入口）同样校验累计，且在写入任何槽之前失败。
        let (_runtime, mut app, window, container) = app_with_container();
        match app.make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: 25.0,
                extents: Some(vec![3e38, 3e38].into_boxed_slice()),
                overscan: 0,
            },
        ) {
            Err(EngineError::InvalidVirtualExtent { index: 1, .. }) => {}
            other => panic!("预置高度累计溢出必须是 InvalidVirtualExtent：{other:?}"),
        }
        let tree = app.window(window).expect("window").tree();
        assert!(
            tree.virtual_slot(container).is_none(),
            "失败入口不得写虚拟槽"
        );
        assert!(
            tree.scroll_slot(container).is_none(),
            "失败入口不得写滚动槽"
        );
    }

    #[test]
    fn an_empty_list_accepts_empty_extents_and_rejects_any_index() {
        let (_rt, mut app, window, container, list) = fixture(25.0, 0, None);
        set_items(&mut app, &list, 0);
        frame(&mut app, window);
        app.set_virtual_extents(&list, &[]).expect("empty extents");
        frame(&mut app, window);
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!(
            (state.count, state.first, state.last, state.mounted),
            (0, 0, 0, 0)
        );
        match app.scroll_to_item(&list, 0, ItemAlign::Nearest) {
            Err(EngineError::VirtualListExtents {
                expected: 0,
                actual: 1,
            }) => {}
            other => panic!("空列表任意 index 都越界：{other:?}"),
        }
        assert_eq!(content_height(&app, window, container), 0.0);
    }

    /// `VirtualListConfig.extents`：条目数匹配时生效；条目数变化时丢弃并按锚点 clamp；
    /// 随后 `set_virtual_extents` 再次保持锚点（§11.3 的两步更新顺序）。
    #[test]
    fn config_extents_are_adopted_and_stale_heights_are_dropped() {
        // 预置 10 项 × 100 高；单一 extent = 200（条目数变化后的回落值）。
        let (_rt, mut app, window, container, list) = fixture(200.0, 0, Some(vec![100.0; 10]));
        set_items(&mut app, &list, 10);
        frame(&mut app, window);
        assert_eq!(content_height(&app, window, container), 1_000.0);
        let handle = app.scroll_handle(window, container).expect("handle");
        app.scroll_to(&handle, Point::new(0.0, 350.0))
            .expect("scroll");
        frame(&mut app, window);
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!((state.first, state.last), (3, 5), "350..450 覆盖第 3、4 行");

        // 条目数变化：旧高度长度不符 ⇒ 丢弃；锚点 f=3、d=50 用 200 的等高行重算 ⇒ 650。
        let requests = app.frame_requests();
        set_items(&mut app, &list, 5);
        assert_eq!(
            app.frame_requests() - requests,
            1,
            "条目数变化 + 锚点补偿也只唤醒一帧（I3）"
        );
        assert_eq!(scroll_offset(&app, window, container), 650.0, "3*200 + 50");
        frame(&mut app, window);
        assert_eq!(content_height(&app, window, container), 1_000.0);
        // F2 回归：丢弃逐项高度 ⇒ 已挂载行几何必须同步回落到单一 `extent`
        // （修复前 row3 仍留着旧逐项布局的 300/100，中间帧把可见行画出视口 ⇒ 列表空白）。
        let mounted = mounted_rows(&app, window, &list);
        assert_eq!(
            mounted.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![3],
            "650..750 只覆盖第 3 行"
        );
        for (index, row) in mounted {
            let bounds = row_bounds(&app, window, container, row);
            assert_eq!(
                bounds.origin.y,
                index as f32 * 200.0,
                "row {index} 回落到单一 extent 的 top"
            );
            assert_eq!(bounds.size.height, 200.0, "row {index} 回落 height");
        }

        // 应用随后给新高度：锚点（uniform 的 f=3、d=50）⇒ prefix'[3] + 50 = 350。
        app.set_virtual_extents(&list, &[100.0; 5])
            .expect("extents");
        assert_eq!(scroll_offset(&app, window, container), 350.0);
        assert_eq!(content_height(&app, window, container), 500.0);

        // 预置高度非法：创建入口就拒绝。
        let (_runtime, mut app, window, container) = app_with_container();
        match app.make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: 25.0,
                extents: Some(vec![25.0, f32::NAN].into_boxed_slice()),
                overscan: 0,
            },
        ) {
            Err(EngineError::InvalidVirtualExtent { index: 1, .. }) => {}
            other => panic!("预置高度非法必须是 InvalidVirtualExtent：{other:?}"),
        }
    }
}
