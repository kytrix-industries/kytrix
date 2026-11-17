//! 虚拟列表：容器、条目窗口重建、状态与行下标查询。
//!
//! `app.rs` 的 `impl App` 只保留同名薄委托，方法体在这里（零行为变更的纯拆分）。

use super::App;
use crate::{
    error::EngineError,
    interaction::{
        ScrollAxis, ScrollConfig, ScrollSlot, VirtualList, VirtualListConfig, VirtualListState,
        VirtualSlot, scroll, virtual_list,
    },
    math::{Point, Size},
    node::{builder::NodeBuilder, dependency::Owner, id::NodeId},
    paint::style::Clip,
    window::WindowId,
};

// ── P10：虚拟列表 ──────────────────────────────────────────────────────────

/// 让一个滚动容器变成虚拟列表（要求 `Clip::Bounds` + `position: relative`）。
///
/// # Errors
/// `StaleNode` / `NotVirtualList`。
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
    slot.virtual_list = Some(VirtualSlot {
        extent: config.extent.max(1.0),
        overscan: config.overscan,
        count: 0,
        builder: None,
        mounted: Vec::new(),
    });
    // 反向索引：`App::rewindow_virtuals` 每帧只需要遍历虚拟列表容器，不再扫全树（R03-1）。
    target.tree_mut().note_virtual_container(container);
    Ok(VirtualList::new(window, container))
}

/// 设置条目总数与行工厂，并重建可见窗口（只在**新建行**时调用 `build`）。
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
    let Some(target) = app.windows.get_mut(&window) else {
        return Ok(());
    };
    let Some(slot) = target.tree_mut().virtual_slot_mut(node) else {
        return Err(EngineError::NotVirtualList { node });
    };
    slot.count = count;
    slot.builder = Some(Box::new(build));
    // 条目总数是容器自身的 a11y 属性（`size_of_set`）：变了必须重发快照。
    target.tree_mut().mark_a11y(node);
    app.rewindow_virtual(window, node);
    Ok(())
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
    let range = virtual_list::visible_range(
        scroll.offset.y,
        viewport.height,
        slot.extent,
        slot.count,
        slot.overscan,
    );
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
/// 消费者：`App::rewindow_virtual`。
pub(crate) fn rewindow_virtual(app: &mut App, window: WindowId, node: NodeId) {
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
    let Some(slot) = target.tree().virtual_slot(node) else {
        return;
    };
    let (extent, overscan, count) = (slot.extent, slot.overscan, slot.count);
    let Some(scroll) = target.tree().scroll_slot(node) else {
        return;
    };
    let viewport = scroll::viewport(target.tree(), node).size;
    let range =
        virtual_list::visible_range(scroll.offset.y, viewport.height, extent, count, overscan);
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
    for index in range {
        let exists = target
            .tree()
            .virtual_slot(node)
            .is_some_and(|slot| slot.mounted.iter().any(|(mounted, _)| *mounted == index));
        if exists {
            continue;
        }
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
        target
            .tree_mut()
            .set_virtual_row(row, index as f32 * extent, extent);
        if let Some(slot) = target.tree_mut().virtual_slot_mut(node) {
            slot.mounted.push((index, row));
        }
        changed = true;
    }
    // 内容尺寸 + 结构脏位
    if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
        slot.content = Size::new(viewport.width, count as f32 * extent);
    }
    if changed {
        target.tree_mut().mark_structure(node);
        app.request_frame();
    }
}
