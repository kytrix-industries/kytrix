use super::App;
use crate::{
    error::EngineError,
    interaction::{
        ItemAlign, ScrollAxis, ScrollConfig, ScrollHandle, ScrollSlot, VirtualList,
        VirtualListConfig, VirtualListState, VirtualSlot, scroll, virtual_list,
    },
    math::{LogicalPoint, LogicalPx, LogicalSize, px},
    node::{builder::NodeBuilder, dependency::Owner, id::NodeId, tree::NodeTree},
    paint::style::Clip,
    window::WindowId,
};

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
        || node.style().inner().position != taffy::style::Position::Relative
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
        offset: LogicalPoint::ZERO,
        content: viewport,
        axis: ScrollAxis::Vertical,
        line_height: ScrollConfig::default().line_height,
    });
    let mut virtual_slot = VirtualSlot::new(config.extent.max(px(1.0)), config.overscan);
    if let Some(extents) = config.extents {
        virtual_slot.set_extents(extents);
    }
    slot.virtual_list = Some(virtual_slot);
    target.tree_mut().note_virtual_container(container);
    Ok(VirtualList::new(window, container))
}

pub(crate) fn set_virtual_items(
    app: &mut App,
    list: &VirtualList,
    count: usize,
    build: impl FnMut(usize) -> NodeBuilder + 'static,
) -> Result<(), EngineError> {
    let window = list.window();
    let node = list.node();
    let Some(target) = app.windows.get(&window) else {
        return Ok(());
    };
    let Some(slot) = target.tree().virtual_slot(node) else {
        return Err(EngineError::NotVirtualList { node });
    };
    let old_count = slot.count;
    let count_changed = count != old_count;
    let Some(scroll_slot) = target.tree().scroll_slot(node) else {
        return Err(EngineError::NotVirtualList { node });
    };
    let offset = scroll_slot.offset.y;
    let viewport_width = scroll::viewport(target.tree(), node).size.width;
    let anchor = virtual_list::offset_anchor(slot, offset);
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
        let tree = target.tree_mut();
        tree.mark_a11y(node);
        if count_changed
            && let Some(root) = crate::a11y::tree::nearest_collection_ancestor(tree, node)
        {
            tree.mark_subtree_a11y(root);
        }
        if anchored.is_some()
            && let Some(scroll) = target.tree_mut().scroll_slot_mut(node)
        {
            scroll.content = LogicalSize::new(viewport_width, content);
        }
        (anchored, extents_dropped)
    };
    app.batch(|app| {
        if let Some(target_offset) = anchored
            && target_offset != offset
        {
            app.scroll_to(
                &ScrollHandle::new(window, node),
                LogicalPoint::new(px(0.0), target_offset),
            )?;
        }
        app.rewindow_virtual(window, node);
        if extents_dropped {
            reposition_mounted_rows(app, window, node);
        }
        Ok(())
    })
}

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

#[must_use]
pub(crate) fn virtual_row_index(app: &App, list: &VirtualList, row: NodeId) -> Option<usize> {
    let target = app.windows.get(&list.window())?;
    let slot = target.tree().virtual_slot(list.node())?;
    slot.mounted
        .iter()
        .find(|(_, mounted)| *mounted == row)
        .map(|(index, _)| *index)
}

pub(crate) fn rewindow_virtuals(app: &mut App, window: WindowId) {
    let containers: Vec<NodeId> = app.windows.get(&window).map_or_else(Vec::new, |target| {
        target.tree().virtual_containers().to_vec()
    });
    for node in containers {
        app.rewindow_virtual(window, node);
    }
}

pub(crate) fn rewindow_virtual(app: &mut App, window: WindowId, node: NodeId) {
    let Some(target) = app.windows.get_mut(&window) else {
        return;
    };
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
    let mut missing: Vec<(usize, LogicalPx, LogicalPx)> = Vec::new();
    if let Some(slot) = target.tree().virtual_slot(node) {
        for index in range.clone() {
            if slot.mounted.iter().any(|(mounted, _)| *mounted == index) {
                continue;
            }
            let (top, height) = virtual_list::row_top_height(slot, index);
            missing.push((index, top, height));
        }
    }
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
    let mut removed: ahash::HashSet<NodeId> = ahash::HashSet::default();
    for (_, row) in &stale {
        let mut ids: Vec<NodeId> = Vec::new();
        target.tree().for_each_subtree(*row, |id| ids.push(id));
        if let Ok(detached) = target.tree_mut().remove(*row) {
            for (removed_node, deps) in detached {
                for signal in deps {
                    app.graph.detach(
                        signal,
                        Owner::Node {
                            window,
                            node: removed_node,
                        },
                    );
                }
            }
            removed.extend(ids);
            changed = true;
        }
    }
    if let Some(slot) = target.tree_mut().virtual_slot_mut(node) {
        slot.mounted.retain(|(index, _)| range.contains(index));
    }
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
    if let Some(slot) = target.tree_mut().scroll_slot_mut(node) {
        slot.content = LogicalSize::new(viewport.width, content);
    }
    if changed {
        target.tree_mut().mark_structure(node);
        app.request_frame();
    }
    if !removed.is_empty() {
        let _ = super::window_set::cleanup_removed_subtree(app, window, &removed);
    }
}

pub(crate) fn set_virtual_extents(
    app: &mut App,
    list: &VirtualList,
    extents: &[LogicalPx],
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
    if slot.extents.as_deref() == Some(extents) {
        return Ok(());
    }
    app.batch(|app| apply_virtual_extents(app, window, node, extents))
}

fn apply_virtual_extents(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    extents: &[LogicalPx],
) -> Result<(), EngineError> {
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
    let (target_offset, offset_changed) = {
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(EngineError::NotVirtualList { node });
        };
        let Some(slot) = target.tree_mut().virtual_slot_mut(node) else {
            return Err(EngineError::NotVirtualList { node });
        };
        slot.set_extents(extents.to_vec().into_boxed_slice());
        let content = virtual_list::content_height(slot);
        let max = (content - viewport.height).max(LogicalPx::ZERO);
        let target_offset = scroll::clamp(
            virtual_list::offset_for_anchor(slot, anchor.0, anchor.1),
            max,
        );
        if let Some(scroll) = target.tree_mut().scroll_slot_mut(node) {
            scroll.content = LogicalSize::new(viewport.width, content);
        }
        (target_offset, target_offset != current_offset)
    };
    if offset_changed {
        app.scroll_to(
            &ScrollHandle::new(window, node),
            LogicalPoint::new(px(0.0), target_offset),
        )?;
    }
    app.rewindow_virtual(window, node);
    reposition_mounted_rows(app, window, node);
    Ok(())
}

fn reposition_mounted_rows(app: &mut App, window: WindowId, node: NodeId) {
    let rows: Vec<(NodeId, LogicalPx, LogicalPx)> = {
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
    let max = (content - viewport.height).max(LogicalPx::ZERO);
    let target_offset = scroll::clamp(proposed, max);
    if target_offset == current {
        return Ok(());
    }
    app.batch(|app| {
        app.scroll_to(
            &ScrollHandle::new(window, node),
            LogicalPoint::new(px(0.0), target_offset),
        )
    })
}

fn validate_extents(extents: &[LogicalPx]) -> Result<(), EngineError> {
    let mut total = LogicalPx::ZERO;
    for (index, value) in extents.iter().copied().enumerate() {
        if !value.is_finite() || value < LogicalPx::ZERO {
            return Err(EngineError::InvalidVirtualExtent { index, value });
        }
        total += value;
        if !total.is_finite() {
            return Err(EngineError::InvalidVirtualExtent { index, value });
        }
    }
    Ok(())
}

fn row_matches(tree: &NodeTree, row: NodeId, top: LogicalPx, height: LogicalPx) -> bool {
    tree.get(row).is_ok_and(|node| {
        let style = node.style().inner();
        style.position == taffy::style::Position::Absolute
            && style.inset.top == taffy::style::LengthPercentageAuto::length(top.value())
            && style.size.height == taffy::style::Dimension::length(height.value())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        math::{LogicalPx, LogicalRect},
        paint::style_builder::StyleBuilder,
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
            size: LogicalSize::new(px(400.0), px(300.0)),
            ..WindowOptions::default()
        });
        let mut style = StyleBuilder::new()
            .size(px(200.0), px(100.0))
            .relative()
            .build();
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

    fn fixture(extent: LogicalPx, overscan: usize, extents: Option<Vec<LogicalPx>>) -> Fixture {
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

    fn scroll_offset(app: &App, window: WindowId, container: NodeId) -> LogicalPx {
        app.scroll_state(&app.scroll_handle(window, container).expect("scroll handle"))
            .expect("scroll state")
            .offset
            .y
    }

    fn content_height(app: &App, window: WindowId, container: NodeId) -> LogicalPx {
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

    fn row_bounds(app: &App, window: WindowId, container: NodeId, row: NodeId) -> LogicalRect {
        let tree = app.window(window).expect("window").tree();
        let origin = tree
            .get(container)
            .expect("container")
            .layout()
            .bounds
            .origin;
        let bounds = tree.get(row).expect("row").layout().bounds;
        LogicalRect::new(
            bounds.origin.x - origin.x,
            bounds.origin.y - origin.y,
            bounds.size.width,
            bounds.size.height,
        )
    }

    #[test]
    fn variable_extents_position_rows_by_the_prefix() {
        let (_rt, mut app, window, container, list) = fixture(px(100.0), 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);

        let mut extents = vec![px(100.0_f32); 100];
        extents[..4].copy_from_slice(&[px(30.0), px(40.0), px(50.0), px(60.0)]);
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
            assert_eq!(bounds.origin.y, px(prefix[index]), "row {index} top");
            assert_eq!(bounds.size.height, extents[index], "row {index} height");
        }
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!((state.first, state.last), (0, 3));
        assert_eq!(
            content_height(&app, window, container),
            px(extents.iter().map(|extent| extent.value()).sum::<f32>())
        );

        app.scroll_to_item(&list, 3, ItemAlign::Start)
            .expect("start");
        assert_eq!(scroll_offset(&app, window, container), px(120.0));
        app.scroll_to_item(&list, 3, ItemAlign::End).expect("end");
        assert_eq!(scroll_offset(&app, window, container), px(80.0));
        app.scroll_to_item(&list, 3, ItemAlign::Center)
            .expect("center");
        assert_eq!(scroll_offset(&app, window, container), px(100.0));
    }

    #[test]
    fn replacing_extents_preserves_the_visible_anchor() {
        let (_rt, mut app, window, container, list) = fixture(px(100.0), 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);
        let handle = app.scroll_handle(window, container).expect("handle");
        app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(150.0)))
            .expect("scroll");
        frame(&mut app, window);
        let before = app.virtual_list_state(&list).expect("state");
        assert_eq!(before.first, 1);
        let inner_before = before.offset.y - 100.0;
        assert_eq!(inner_before, px(50.0));

        let mut extents = vec![px(100.0_f32); 100];
        extents[0] = px(200.0);
        let requests = app.frame_requests();
        app.set_virtual_extents(&list, &extents).expect("extents");
        assert_eq!(
            app.frame_requests() - requests,
            1,
            "一次 extents 替换 ⇒ 恰好一次唤醒（I3）"
        );
        frame(&mut app, window);

        let after = app.virtual_list_state(&list).expect("state");
        assert_eq!(after.offset.y, px(250.0), "prefix'[1] + 50");
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
                px(prefix[index]),
                "row {index} 已按新 prefix 重排"
            );
        }
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn replacing_extents_paints_one_partial_frame() {
        let (_rt, mut app, window, _container, list) = fixture(px(25.0), 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);
        let mut extents = vec![px(25.0_f32); 100];
        extents[0] = px(50.0);
        app.set_virtual_extents(&list, &extents).expect("extents");
        frame(&mut app, window);
        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("extents 替换必须提交一帧");
        assert!(record.repainted, "必要条件必须重绘");
        assert!(!record.damage_full, "只重画可见行/视口，不退化为整帧");
        let outcome = app.frame(window).expect("window").expect("frame");
        assert_eq!(outcome, crate::window::FrameOutcome::Idle);
    }

    #[test]
    fn scroll_to_item_follows_the_four_alignments() {
        let (_rt, mut app, window, container, list) = fixture(px(20.0), 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);

        app.scroll_to_item(&list, 10, ItemAlign::Start)
            .expect("start");
        assert_eq!(scroll_offset(&app, window, container), px(200.0));
        app.scroll_to_item(&list, 10, ItemAlign::Center)
            .expect("center");
        assert_eq!(scroll_offset(&app, window, container), px(160.0));
        app.scroll_to_item(&list, 10, ItemAlign::End).expect("end");
        assert_eq!(scroll_offset(&app, window, container), px(120.0));

        let requests = app.frame_requests();
        app.scroll_to_item(&list, 10, ItemAlign::Nearest)
            .expect("nearest");
        assert_eq!(scroll_offset(&app, window, container), px(120.0));
        assert_eq!(app.frame_requests(), requests, "已可见 ⇒ 无帧");
        app.scroll_to(
            &app.scroll_handle(window, container).expect("handle"),
            LogicalPoint::new(px(0.0), px(130.0)),
        )
        .expect("scroll");
        app.scroll_to_item(&list, 0, ItemAlign::Nearest)
            .expect("nearest up");
        assert_eq!(scroll_offset(&app, window, container), px(0.0));
        app.scroll_to_item(&list, 99, ItemAlign::Nearest)
            .expect("nearest down");
        assert_eq!(scroll_offset(&app, window, container), px(1_900.0));
        app.scroll_to_item(&list, 99, ItemAlign::End).expect("end");
        assert_eq!(scroll_offset(&app, window, container), px(1_900.0));
    }

    #[test]
    fn an_identical_extents_replacement_never_requests_a_frame() {
        let (_rt, mut app, window, container, list) = fixture(px(25.0), 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);
        let extents = vec![px(25.0_f32); 100];
        let requests = app.frame_requests();
        app.set_virtual_extents(&list, &extents).expect("extents");
        assert_eq!(
            app.frame_requests(),
            requests,
            "与单一 extent 等价 ⇒ 可见内容未变"
        );
        app.set_virtual_extents(&list, &extents).expect("extents");
        assert_eq!(app.frame_requests(), requests, "同值替换是 no-op");
        assert_eq!(content_height(&app, window, container), px(2_500.0));
    }

    #[test]
    fn offscreen_extents_changes_do_not_force_a_frame() {
        let (_rt, mut app, window, container, list) = fixture(px(25.0), 0, None);
        set_items(&mut app, &list, 100);
        frame(&mut app, window);
        let mut extents = vec![px(25.0_f32); 100];
        extents[99] = px(100.0);
        let requests = app.frame_requests();
        app.set_virtual_extents(&list, &extents).expect("extents");
        assert_eq!(app.frame_requests(), requests, "可见行未移动 ⇒ 不请求帧");
        assert_eq!(content_height(&app, window, container), px(2_575.0));
    }

    #[test]
    fn extents_and_item_index_errors_are_typed() {
        let (_rt, mut app, window, container, list) = fixture(px(25.0), 0, None);
        set_items(&mut app, &list, 10);
        frame(&mut app, window);

        match app.set_virtual_extents(&list, &[px(25.0); 9]) {
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
        let mut nan = vec![px(25.0_f32); 10];
        nan[3] = px(f32::NAN);
        match app.set_virtual_extents(&list, &nan) {
            Err(EngineError::InvalidVirtualExtent { index: 3, value }) => {
                assert!(value.value().is_nan());
            }
            other => panic!("NaN 必须是 InvalidVirtualExtent：{other:?}"),
        }
        let mut negative = vec![px(25.0_f32); 10];
        negative[0] = px(-1.0);
        match app.set_virtual_extents(&list, &negative) {
            Err(EngineError::InvalidVirtualExtent { index: 0, value }) => {
                assert_eq!(value, px(-1.0));
            }
            other => panic!("负值必须是 InvalidVirtualExtent：{other:?}"),
        }
        match app.set_virtual_extents(&list, &[px(f32::INFINITY); 10]) {
            Err(EngineError::InvalidVirtualExtent { index: 0, .. }) => {}
            other => panic!("非有限值必须是 InvalidVirtualExtent：{other:?}"),
        }
        match app.scroll_to_item(&list, 10, ItemAlign::Start) {
            Err(EngineError::VirtualListExtents {
                expected: 10,
                actual: 11,
            }) => {}
            other => panic!("index 越界必须是 VirtualListExtents：{other:?}"),
        }
        app.set_virtual_extents(&list, &[px(0.0); 10])
            .expect("zero heights");
        frame(&mut app, window);
        assert_eq!(content_height(&app, window, container), px(0.0));
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!((state.first, state.last, state.mounted), (0, 0, 0));

        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .remove(container)
            .expect("remove");
        match app.set_virtual_extents(&list, &[px(25.0); 10]) {
            Err(EngineError::NotVirtualList { node }) => assert_eq!(node, container),
            other => panic!("失效 list 必须是 NotVirtualList：{other:?}"),
        }
        match app.scroll_to_item(&list, 0, ItemAlign::Start) {
            Err(EngineError::NotVirtualList { node }) => assert_eq!(node, container),
            other => panic!("失效 list 必须是 NotVirtualList：{other:?}"),
        }
    }

    #[test]
    fn zero_heights_then_real_heights_keep_the_visible_anchor_at_the_top() {
        let (_rt, mut app, window, container, list) = fixture(px(100.0), 0, None);
        set_items(&mut app, &list, 10);
        frame(&mut app, window);
        app.set_virtual_extents(&list, &[px(0.0); 10])
            .expect("zero heights");
        frame(&mut app, window);
        assert_eq!(content_height(&app, window, container), px(0.0));
        let folded = app.virtual_list_state(&list).expect("state");
        assert_eq!(
            (folded.offset.y, folded.first, folded.mounted),
            (px(0.0), 0, 0),
            "零内容：无可见行、偏移 0"
        );

        app.set_virtual_extents(&list, &[px(100.0); 10])
            .expect("measured heights");
        frame(&mut app, window);
        let measured = app.virtual_list_state(&list).expect("state");
        assert_eq!(
            (measured.offset.y, measured.first),
            (px(0.0), 0),
            "零高 → 真实高度不得跳底"
        );
        assert_eq!(content_height(&app, window, container), px(1_000.0));
    }

    #[test]
    fn overflowing_extent_totals_are_rejected_before_any_state_change() {
        let (_rt, mut app, window, container, list) = fixture(px(25.0), 0, None);
        set_items(&mut app, &list, 3);
        frame(&mut app, window);
        let before = app.virtual_list_state(&list).expect("state");
        let requests = app.frame_requests();
        match app.set_virtual_extents(&list, &[px(3e38), px(3e38), px(1.0)]) {
            Err(EngineError::InvalidVirtualExtent { index: 1, value }) => {
                assert_eq!(value, px(3e38));
            }
            other => panic!("累计溢出必须是 InvalidVirtualExtent：{other:?}"),
        }
        assert_eq!(
            content_height(&app, window, container),
            px(75.0),
            "错误路径不得半写 content"
        );
        assert_eq!(
            app.virtual_list_state(&list).expect("state"),
            before,
            "错误路径不改状态（先校验、后写入）"
        );
        assert_eq!(app.frame_requests(), requests, "错误路径不请求帧");
        validate_extents(&[px(3e38), px(1.0), px(1.0)]).expect("finite total");

        let (_runtime, mut app, window, container) = app_with_container();
        match app.make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(25.0),
                extents: Some(vec![px(3e38), px(3e38)].into_boxed_slice()),
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
        let (_rt, mut app, window, container, list) = fixture(px(25.0), 0, None);
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
        assert_eq!(content_height(&app, window, container), px(0.0));
    }

    #[test]
    fn config_extents_are_adopted_and_stale_heights_are_dropped() {
        let (_rt, mut app, window, container, list) =
            fixture(px(200.0), 0, Some(vec![px(100.0); 10]));
        set_items(&mut app, &list, 10);
        frame(&mut app, window);
        assert_eq!(content_height(&app, window, container), px(1_000.0));
        let handle = app.scroll_handle(window, container).expect("handle");
        app.scroll_to(&handle, LogicalPoint::new(px(0.0), px(350.0)))
            .expect("scroll");
        frame(&mut app, window);
        let state = app.virtual_list_state(&list).expect("state");
        assert_eq!((state.first, state.last), (3, 5), "350..450 覆盖第 3、4 行");

        let requests = app.frame_requests();
        set_items(&mut app, &list, 5);
        assert_eq!(
            app.frame_requests() - requests,
            1,
            "条目数变化 + 锚点补偿也只唤醒一帧（I3）"
        );
        assert_eq!(
            scroll_offset(&app, window, container),
            px(650.0),
            "3*200 + 50"
        );
        frame(&mut app, window);
        assert_eq!(content_height(&app, window, container), px(1_000.0));
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
                px(index as f32 * 200.0),
                "row {index} 回落到单一 extent 的 top"
            );
            assert_eq!(bounds.size.height, px(200.0), "row {index} 回落 height");
        }

        app.set_virtual_extents(&list, &[px(100.0); 5])
            .expect("extents");
        assert_eq!(scroll_offset(&app, window, container), px(350.0));
        assert_eq!(content_height(&app, window, container), px(500.0));

        let (_runtime, mut app, window, container) = app_with_container();
        match app.make_virtual_list(
            window,
            container,
            VirtualListConfig {
                extent: px(25.0),
                extents: Some(vec![px(25.0), px(f32::NAN)].into_boxed_slice()),
                overscan: 0,
            },
        ) {
            Err(EngineError::InvalidVirtualExtent { index: 1, .. }) => {}
            other => panic!("预置高度非法必须是 InvalidVirtualExtent：{other:?}"),
        }
    }

    #[cfg(feature = "test-support")]
    struct TableFixture {
        _runtime: tokio::runtime::Runtime,
        app: App,
        window: WindowId,
        table: NodeId,
        header_row: NodeId,
        header_cell: NodeId,
        viewport: NodeId,
        list: VirtualList,
        fillers: Vec<NodeId>,
    }

    #[cfg(feature = "test-support")]
    fn table_fixture() -> TableFixture {
        use crate::a11y::{A11yOverride, A11yRole};

        fn fixed_style(width: f32, height: f32) -> crate::paint::style::Style {
            StyleBuilder::new().size(px(width), px(height)).build()
        }
        fn role(role: A11yRole) -> A11yOverride {
            A11yOverride {
                role: Some(role),
                ..A11yOverride::default()
            }
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: "e9c".into(),
            size: LogicalSize::new(px(400.0), px(300.0)),
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        let (table, header_row, header_cell, viewport, fillers) = {
            let tree = app.window_mut(window).expect("window").tree_mut();
            let mut table_style = fixed_style(200.0, 200.0);
            table_style.flex_direction(taffy::style::FlexDirection::Column);
            let table = NodeBuilder::boxed()
                .a11y(role(A11yRole::Table))
                .style(table_style)
                .build(tree, root)
                .expect("table");
            let header_box = NodeBuilder::boxed()
                .style(fixed_style(200.0, 20.0))
                .build(tree, table)
                .expect("header box");
            let header_row = NodeBuilder::boxed()
                .a11y(role(A11yRole::Row))
                .style(fixed_style(200.0, 20.0))
                .build(tree, header_box)
                .expect("header row");
            let header_cell = NodeBuilder::boxed()
                .a11y(A11yOverride {
                    role: Some(A11yRole::ColumnHeader),
                    label: Some(smol_str::SmolStr::new_static("价格")),
                    ..A11yOverride::default()
                })
                .style(fixed_style(100.0, 20.0))
                .build(tree, header_row)
                .expect("header cell");
            let mut viewport_style = fixed_style(200.0, 60.0);
            viewport_style.position(taffy::style::Position::Relative);
            viewport_style.clip = Clip::Bounds;
            let viewport = NodeBuilder::boxed()
                .style(viewport_style)
                .build(tree, table)
                .expect("viewport");
            let fillers = (0..20)
                .map(|index| {
                    NodeBuilder::boxed()
                        .layout_override(LogicalRect::new(
                            px(0.0),
                            px(240.0 + index as f32 * 2.0),
                            px(10.0),
                            px(2.0),
                        ))
                        .build(tree, root)
                        .expect("filler")
                })
                .collect();
            (table, header_row, header_cell, viewport, fillers)
        };
        frame(&mut app, window);
        let list = app
            .make_virtual_list(
                window,
                viewport,
                VirtualListConfig {
                    extent: px(20.0),
                    overscan: 0,
                    extents: None,
                },
            )
            .expect("virtual list");
        app.set_virtual_items(&list, 2, row_item_builder)
            .expect("items");
        frame(&mut app, window);
        TableFixture {
            _runtime: runtime,
            app,
            window,
            table,
            header_row,
            header_cell,
            viewport,
            list,
            fillers,
        }
    }

    #[cfg(feature = "test-support")]
    fn row_item_builder(index: usize) -> NodeBuilder {
        NodeBuilder::boxed()
            .id(format!("row{index}"))
            .a11y(crate::a11y::A11yOverride {
                role: Some(crate::a11y::A11yRole::Row),
                ..crate::a11y::A11yOverride::default()
            })
    }

    #[cfg(feature = "test-support")]
    fn take_a11y(app: &mut App, window: WindowId) -> Vec<crate::a11y::A11yTreeSnapshot> {
        let mut batches = Vec::new();
        while let Some(snapshot) = app.a11y_take(window) {
            batches.push(snapshot);
        }
        batches
    }

    #[cfg(feature = "test-support")]
    fn a11y_record(
        batches: &[crate::a11y::A11yTreeSnapshot],
        node: NodeId,
    ) -> Option<&crate::a11y::A11yNode> {
        batches.iter().find_map(|snapshot| snapshot.node_for(node))
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn count_change_marks_the_collection_subtree_a11y_dirty() {
        use crate::{
            a11y::adapter::to_tree_update,
            test_support::a11y::{ShadowTree, assert_contract_holds},
        };

        let TableFixture {
            _runtime,
            mut app,
            window,
            table,
            header_row,
            header_cell,
            viewport,
            list,
            fillers,
        } = table_fixture();

        let mut shadow = ShadowTree::new();
        let full = take_a11y(&mut app, window);
        assert!(
            full.first().is_some_and(|snapshot| snapshot.full),
            "首笔必须是全树"
        );
        for snapshot in &full {
            assert_contract_holds(&mut shadow, &to_tree_update(snapshot));
        }
        assert_eq!(
            a11y_record(&full, header_row).expect("表头行").row_count,
            Some(3),
            "1 表头行 + 2 条目"
        );
        assert_eq!(
            a11y_record(&full, header_cell)
                .expect("表头单元格")
                .row_count,
            Some(3),
            "表头单元格的行数同样来自集合推导"
        );
        assert_eq!(a11y_record(&full, table).expect("表").row_count, Some(3));
        assert_eq!(
            a11y_record(&full, viewport).expect("行窗口").size_of_set,
            Some(2),
            "集合容器的 size_of_set = slot.count"
        );

        let requests = app.frame_requests();
        app.set_virtual_items(&list, 10, row_item_builder)
            .expect("items");
        assert!(app.frame_requests() > requests, "条目数变化必须唤醒一帧");
        frame(&mut app, window);
        let incremental = take_a11y(&mut app, window);
        assert!(!incremental.is_empty(), "条目数变化必须产出 a11y 交付");
        assert!(
            incremental.iter().all(|snapshot| !snapshot.full),
            "count 变化只该产生增量笔，不得全树重建"
        );
        for snapshot in &incremental {
            assert_contract_holds(&mut shadow, &to_tree_update(snapshot));
        }

        assert_eq!(
            a11y_record(&incremental, header_row)
                .expect("表头行必须重进增量笔（集合子树脏标记）")
                .row_count,
            Some(11),
            "1 表头行 + 10 条目"
        );
        assert_eq!(
            a11y_record(&incremental, header_cell)
                .expect("表头单元格必须重进增量笔")
                .row_count,
            Some(11)
        );
        assert_eq!(
            a11y_record(&incremental, table).expect("表").row_count,
            Some(11)
        );
        assert_eq!(
            a11y_record(&incremental, viewport)
                .expect("行窗口")
                .size_of_set,
            Some(10)
        );

        let tree_len = app.window(window).expect("window").tree().len();
        let delivered: usize = incremental
            .iter()
            .map(|snapshot| snapshot.nodes.len())
            .sum();
        assert!(
            delivered < tree_len,
            "增量笔不得退化为全树：重发 {delivered} / 共 {tree_len}"
        );
        assert!(
            fillers
                .iter()
                .all(|filler| a11y_record(&incremental, *filler).is_none()),
            "无关填充子树不得被重发"
        );

        let (index, row) = mounted_rows(&app, window, &list)
            .into_iter()
            .find(|(index, _)| *index == 2)
            .expect("count=10、视口 60/行高 20 ⇒ 挂载第 0..3 行");
        let record = a11y_record(&incremental, row).expect("新挂载行必须进增量笔");
        assert_eq!(
            record.row_index,
            Some(1 + index),
            "数据行逻辑行号含表头偏移"
        );
        assert_eq!(record.row_count, Some(11));
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn an_unchanged_count_keeps_the_collection_a11y_dirty_set_minimal() {
        let TableFixture {
            _runtime,
            mut app,
            window,
            header_row,
            viewport,
            list,
            ..
        } = table_fixture();
        let _ = take_a11y(&mut app, window);
        assert!(
            take_a11y(&mut app, window).is_empty(),
            "交付后不得残留 a11y 工作"
        );

        let requests = app.frame_requests();
        app.set_virtual_items(&list, 2, row_item_builder)
            .expect("items");
        assert_eq!(app.frame_requests(), requests, "条目数不变 ⇒ 不出帧（I3）");

        let incremental = take_a11y(&mut app, window);
        assert!(
            !incremental.is_empty(),
            "窗口节点自身仍会标 A11Y（既有语义）"
        );
        assert!(incremental.iter().all(|snapshot| !snapshot.full));
        assert!(
            a11y_record(&incremental, header_row).is_none(),
            "条目数不变不得把集合子树标脏"
        );
        assert_eq!(
            a11y_record(&incremental, viewport)
                .expect("窗口节点")
                .size_of_set,
            Some(2)
        );
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn count_change_without_a_collection_root_does_not_dirty_the_whole_tree() {
        let (_runtime, mut app, window, _container, list) = fixture(px(20.0), 0, None);
        set_items(&mut app, &list, 2);
        frame(&mut app, window);
        let _ = take_a11y(&mut app, window);

        set_items(&mut app, &list, 10);
        frame(&mut app, window);
        let incremental = take_a11y(&mut app, window);
        assert!(!incremental.is_empty());
        let tree_len = app.window(window).expect("window").tree().len();
        let delivered: usize = incremental
            .iter()
            .map(|snapshot| snapshot.nodes.len())
            .sum();
        assert!(
            delivered <= 6,
            "裸列表只标窗口 + 新挂载行，不得整树标脏（重发 {delivered} / 共 {tree_len}）"
        );
    }
}
