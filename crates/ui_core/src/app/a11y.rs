use smallvec::SmallVec;

use super::{A11Y_DIAGNOSTIC_CAPACITY, A11Y_THROTTLE, App};
use crate::{
    node::dependency::{Owner, ReadCtx, ReadTracker},
    platform::dispatcher::Dispatcher,
    window::{Window, WindowId},
};

pub(crate) fn on_a11y_action(
    app: &mut App,
    action: crate::a11y::A11yAction,
    handler: impl FnMut(&mut crate::a11y::A11yActionCtx<'_>) + 'static,
) -> bool {
    if action == crate::a11y::A11yAction::Click {
        return false;
    }
    app.a11y_handlers.insert(action, Box::new(handler));
    true
}

pub(crate) fn off_a11y_action(app: &mut App, action: crate::a11y::A11yAction) -> bool {
    app.a11y_handlers.remove(&action).is_some()
}

#[must_use]
pub(crate) fn a11y_diagnostics(app: &App) -> &[crate::a11y::A11yDiagnostic] {
    &app.a11y_diagnostics
}

pub(crate) fn a11y_note(app: &mut App, diagnostic: crate::a11y::A11yDiagnostic) {
    if app.a11y_diagnostics.contains(&diagnostic) {
        return;
    }
    if app.a11y_diagnostics.len() >= A11Y_DIAGNOSTIC_CAPACITY {
        app.a11y_diagnostics.remove(0);
    }
    app.a11y_diagnostics.push(diagnostic);
}

pub(crate) fn a11y_has_handler(app: &App, action: crate::a11y::A11yAction) -> bool {
    app.a11y_handlers.contains_key(&action)
}

pub(crate) fn a11y_take_handler(
    app: &mut App,
    action: crate::a11y::A11yAction,
) -> Option<Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>> {
    app.a11y_handlers.remove(&action)
}

pub(crate) fn a11y_put_handler(
    app: &mut App,
    action: crate::a11y::A11yAction,
    handler: Box<dyn FnMut(&mut crate::a11y::A11yActionCtx<'_>)>,
) {
    app.a11y_handlers.insert(action, handler);
}

#[must_use]
pub(crate) fn a11y_pending(app: &App, window: WindowId) -> bool {
    let Some(target) = app.windows.get(&window) else {
        return false;
    };
    if !target.a11y_pending() {
        return false;
    }
    target.a11y_focus_pending()
        || target.a11y_consumer_epoch_pending()
        || target.a11y_rebuild_pending()
        || target.a11y_backlog_pending()
        || app
            .a11y_delivered
            .get(&window)
            .is_none_or(|last| app.clock.saturating_duration_since(*last) >= A11Y_THROTTLE)
}

#[must_use]
pub(crate) fn a11y_backlog_pending(app: &App) -> bool {
    app.windows.values().any(Window::a11y_backlog_pending)
}

pub(crate) fn wake_for_a11y_backlog(app: &mut App, dispatcher: &Dispatcher) {
    let delivered = std::mem::take(&mut app.a11y_delivered_round);
    if delivered && app.a11y_backlog_pending() {
        dispatcher.wake();
    }
}

pub(crate) fn a11y_retry_pending(app: &App) -> bool {
    app.windows.keys().any(|window| {
        app.windows.get(window).is_some_and(Window::a11y_pending)
            && app
                .a11y_delivered
                .get(window)
                .is_some_and(|last| app.clock.saturating_duration_since(*last) < A11Y_THROTTLE)
    })
}

#[must_use]
pub(crate) fn window_a11y_epoch(
    app: &App,
    window: WindowId,
) -> Option<crate::a11y::adapter::A11yConsumerEpoch> {
    app.windows.get(&window).map(Window::a11y_consumer_epoch)
}

pub(crate) fn a11y_take(app: &mut App, window: WindowId) -> Option<crate::a11y::A11yTreeSnapshot> {
    let mut diagnostics = Vec::new();
    let snapshot = {
        let App { windows, .. } = app;
        windows.get_mut(&window)?.a11y_take(&mut diagnostics)
    };
    for diagnostic in diagnostics {
        app.a11y_note(diagnostic);
    }
    if snapshot.is_some() {
        app.a11y_delivered.insert(window, app.clock);
        app.a11y_delivered_round = true;
    }
    snapshot
}

pub(crate) fn set_a11y_patch(
    app: &mut App,
    window: WindowId,
    node: crate::node::id::NodeId,
    patch: crate::a11y::A11yPatch,
) -> Result<(), crate::error::EngineError> {
    {
        let Some(target) = app.windows.get_mut(&window) else {
            return Err(crate::error::EngineError::UnknownWindow { window });
        };
        let tree = target.tree_mut();
        tree.get(node)?;
        crate::a11y::tree::write_a11y_patch(tree, node, patch);
    }
    app.request_frame();
    Ok(())
}

pub(crate) fn refresh_a11y_bindings(app: &mut App, window: WindowId) {
    if !app
        .windows
        .get(&window)
        .is_some_and(Window::a11y_binding_work)
    {
        return;
    }
    let App {
        store,
        graph,
        windows,
        eval_stack,
        ..
    } = app;
    debug_assert!(eval_stack.is_empty(), "上一帧的派生求值栈泄漏");
    eval_stack.clear();
    let Some(target) = windows.get_mut(&window) else {
        return;
    };
    let nodes = target.a11y_take_bindings();
    let tree = target.tree_mut();
    for node in nodes.iter().copied() {
        if tree.get(node).is_err() {
            continue;
        }
        let Some(bind) = tree.take_bind_a11y(node) else {
            let ghost_deps = tree.deps(node).unwrap_or(&[]);
            graph.replace_deps(Owner::Node { window, node }, ghost_deps, &[]);
            tree.set_deps(node, SmallVec::new());
            continue;
        };
        let (values, derived) = store.split();
        let mut tracker = ReadTracker::new();
        let (value, owner) = {
            let mut ctx = ReadCtx::new(
                values,
                derived,
                graph,
                &mut tracker,
                &mut *eval_stack,
                Owner::Node { window, node },
            );
            (bind(&mut ctx), ctx.owner())
        };
        let old_deps = tree.deps(node).unwrap_or(&[]);
        graph.replace_deps(owner, old_deps, tracker.as_slice());
        tree.set_deps(node, tracker.into_inner());
        let _ = crate::a11y::tree::write_a11y_patch(tree, node, value);
        tree.put_bind_a11y(node, bind);
    }
    target.a11y_recycle_bindings(nodes);
}

#[cfg(feature = "test-support")]
pub(crate) fn set_a11y_capture(app: &mut App, window: WindowId, on: bool) {
    if let Some(target) = app.windows.get_mut(&window) {
        target.set_a11y_capture(on);
    }
}

#[cfg(feature = "test-support")]
pub(crate) fn attach_a11y(
    app: &mut App,
    window: WindowId,
    snapshot: crate::a11y::A11yTreeSnapshot,
) {
    if let Some(target) = app.windows.get_mut(&window) {
        target.attach_a11y(snapshot);
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use smol_str::SmolStr;

    use crate::{
        a11y::{A11yDiagnostic, A11yOverride, A11yPatch, A11yRole},
        app::App,
        math::{DevicePixelRatio, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, signal::Signal},
        paint::{
            color::Color,
            style::{PaintSpec, StylePatch},
        },
        window::{FrameOutcome, WindowId, WindowOptions},
    };

    fn white() -> PaintSpec {
        PaintSpec {
            background: Color::WHITE,
            ..PaintSpec::default()
        }
    }

    fn fixture() -> (
        tokio::runtime::Runtime,
        App,
        WindowId,
        Signal<SmolStr>,
        NodeId,
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: SmolStr::new_static("a11y-only-cost"),
            size: LogicalSize::new(px(320.0), px(240.0)),
            scale: DevicePixelRatio::ONE,
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        for index in 0..10 {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(
                    px(0.0),
                    px(index as f32 * 6.0),
                    px(40.0),
                    px(5.0),
                ))
                .paint(white())
                .build(app.window_mut(window).expect("window").tree_mut(), root)
                .expect("static box");
        }
        let label_signal = app.signal(SmolStr::new_static("旧的"));
        let source = label_signal;
        let label = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(80.0), px(120.0), px(20.0)))
            .paint(white())
            .bind_a11y(move |cx| A11yPatch {
                label: Some(cx.read(&source).clone()),
                ..A11yPatch::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("a11y label");
        (runtime, app, window, label_signal, label)
    }

    #[test]
    fn an_a11y_only_change_does_not_repaint() {
        let (_runtime, mut app, window, label_signal, label) = fixture();
        let first = app.frame(window).expect("window").expect("first frame");
        assert!(
            matches!(first, FrameOutcome::Committed(_)),
            "首帧必须提交（实际 {first:?}）"
        );
        assert!(app.a11y_take(window).is_some(), "首笔 a11y 交付必为全树");
        let before = app.window(window).expect("window").frame_index();

        app.write(&label_signal, SmolStr::new_static("新的"));
        assert!(
            app.window(window).expect("window").a11y_binding_work(),
            "写 a11y 绑定后必须登记待重算（不得走像素通道）"
        );
        assert!(
            app.window(window).expect("window").a11y_pending(),
            "写 a11y 绑定后必须有待投递的 a11y 工作（限流只管投递时机，不管 patch 重算）"
        );

        let outcome = app.frame(window).expect("window").expect("a11y-only frame");
        assert!(
            matches!(outcome, FrameOutcome::Idle),
            "a11y-only 帧不得提交像素（实际 {outcome:?}）"
        );

        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("record");
        assert_eq!(
            record.frame_index, before,
            "a11y-only 帧不得推进 FrameIndex"
        );
        assert!(record.a11y_only, "本帧必须被判定为 a11y-only");
        assert!(!record.repainted, "a11y-only 帧不得发生任何重绘");
        assert!(!record.rendered, "a11y-only 帧不提交像素");
        assert!(record.frame.is_none(), "不得产生 RenderedFrame");
        assert!(record.damage.is_empty(), "a11y-only 帧不得产生 damage");
        assert_eq!(record.stats.painted_lanes, 0, "不得重建任何 lane");
        assert_eq!(record.stats.encoded_lanes, 0, "不得重编码任何 lane");
        assert!(
            record.paint_log.lanes.is_empty(),
            "paint pass 不得访问任何 lane（实际 {:?}）",
            record.paint_log.lanes
        );
        assert!(record.dirty_nodes.is_empty(), "不得有节点被重绘");
        assert_eq!(
            record.stats.a11y_only_frames, 1,
            "a11y-only 快速路径必须记一笔（否则帧可能只是碰巧不必画）"
        );

        let snapshot = app.a11y_take(window).expect("a11y 交付必须发生");
        assert!(
            !snapshot.full,
            "第二笔必须是增量（只重发变化节点 + 祖先链）"
        );
        assert_eq!(
            snapshot.node_for(label).expect("label 节点").label_text(),
            Some("新的"),
            "a11y 树必须拿到新 label（不重绘 ≠ 不交付）"
        );
    }

    #[test]
    fn a_paint_frame_also_refreshes_a11y_bindings() {
        let (_runtime, mut app, window, label_signal, label) = fixture();
        let root = app.window(window).expect("window").tree().root();
        let shared = label_signal;
        let styled = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(110.0), px(40.0), px(20.0)))
            .paint(white())
            .bind_style(move |cx| StylePatch {
                opacity: Some(if cx.read(&shared).as_str() == "新的" {
                    0.5
                } else {
                    1.0
                }),
                ..StylePatch::default()
            })
            .build(app.window_mut(window).expect("window").tree_mut(), root)
            .expect("styled box");
        let first = app.frame(window).expect("window").expect("first frame");
        assert!(matches!(first, FrameOutcome::Committed(_)));
        let _ = app.a11y_take(window).expect("首笔 a11y 交付");

        app.write(&label_signal, SmolStr::new_static("新的"));
        let outcome = app.frame(window).expect("window").expect("paint frame");
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "有 style 绑定的同一批写必须落成绘制帧（实际 {outcome:?}）"
        );
        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("record");
        assert!(!record.a11y_only, "像素帧不得被误判成 a11y-only");
        assert_eq!(
            record.stats.a11y_only_frames, 0,
            "像素帧不得计入 a11y-only 帧"
        );

        let snapshot = app.a11y_take(window).expect("a11y 交付");
        assert_eq!(
            snapshot.node_for(label).expect("label 节点").label_text(),
            Some("新的"),
            "同一帧里 bind_a11y 必须已重算"
        );
        let _ = styled;
    }

    #[test]
    fn a_consumer_epoch_reset_bypasses_the_a11y_throttle() {
        let (_runtime, mut app, window, _label_signal, label) = fixture();
        let _ = app.frame(window).expect("window").expect("first frame");
        assert!(app.a11y_take(window).is_some(), "首笔 a11y 交付必为全树");

        app.window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_a11y(label);
        assert!(
            !app.a11y_pending(window),
            "限流窗口内的普通脏位不得立即交付（否则旁路测试没有意义）"
        );

        let epoch = app.window(window).expect("window").a11y_consumer_epoch();
        epoch.bump();
        assert!(
            app.a11y_pending(window),
            "消费端重置必须走不节流旁路（AT 此时看到空树，等 100 ms 没有意义）"
        );
        let snapshot = app.a11y_take(window).expect("重置后的第一笔必须产出");
        assert!(snapshot.full, "重置后的第一笔必须是全树");
    }

    #[test]
    fn set_a11y_patch_requests_an_a11y_only_frame() {
        let (_runtime, mut app, window, _label_signal, label) = fixture();
        let first = app.frame(window).expect("window").expect("first frame");
        assert!(matches!(first, FrameOutcome::Committed(_)));
        let _ = app.a11y_take(window).expect("首笔 a11y 交付");

        app.set_a11y_patch(
            window,
            label,
            A11yPatch {
                label: Some(SmolStr::new_static("新的")),
                ..A11yPatch::default()
            },
        )
        .expect("a11y patch");

        let outcome = app.frame(window).expect("window").expect("frame");
        assert!(
            matches!(outcome, FrameOutcome::Idle),
            "A11Y 变化不得提交像素（实际 {outcome:?}）"
        );
        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("record");
        assert_eq!(
            record.stats.a11y_only_frames, 1,
            "set_a11y_patch 必须请求一帧并走 a11y-only 快路径"
        );
        assert!(!record.repainted, "a11y-only 帧不得重绘");
        let snapshot = app.a11y_take(window).expect("a11y 交付");
        assert!(!snapshot.full, "补丁必须是增量笔");
        assert_eq!(
            snapshot.node_for(label).expect("label").label_text(),
            Some("新的"),
            "请求到的帧必须把补丁带进交付"
        );
    }

    #[test]
    fn set_a11y_patch_reports_unknown_window_and_stale_node() {
        let (_runtime, mut app, window, _label_signal, label) = fixture();
        let patch = A11yPatch {
            label: Some(SmolStr::new_static("x")),
            ..A11yPatch::default()
        };
        let unknown = WindowId::for_test(999);
        assert!(matches!(
            app.set_a11y_patch(unknown, label, patch.clone()),
            Err(crate::error::EngineError::UnknownWindow { window: reported }) if reported == unknown
        ));
        let stale = NodeId::for_test(999, 1);
        assert!(matches!(
            app.set_a11y_patch(window, stale, patch),
            Err(crate::error::EngineError::StaleNode { node }) if node == stale
        ));
    }

    #[test]
    fn new_roles_without_a_name_are_diagnosed() {
        let (_runtime, mut app, window, _label_signal, _label) = fixture();
        let roles = [
            A11yRole::ComboBox,
            A11yRole::ListBox,
            A11yRole::MenuItemCheckBox,
            A11yRole::MenuItemRadio,
            A11yRole::ColorWell,
        ];
        let mut nodes = Vec::with_capacity(roles.len());
        {
            let tree = app.window_mut(window).expect("window").tree_mut();
            let root = tree.root();
            for role in roles {
                nodes.push(
                    NodeBuilder::boxed()
                        .a11y(A11yOverride {
                            role: Some(role),
                            ..A11yOverride::default()
                        })
                        .build(tree, root)
                        .expect("role node"),
                );
            }
        }
        let _ = app.frame(window).expect("window").expect("frame");
        let _ = app.a11y_take(window).expect("a11y delivery");
        for (role, node) in roles.into_iter().zip(nodes) {
            assert!(
                app.a11y_diagnostics().iter().any(|diagnostic| matches!(
                    diagnostic,
                    A11yDiagnostic::MissingLabel { node: reported } if *reported == node
                )),
                "角色 {role:?} 没有名字必须记 MissingLabel：{:?}",
                app.a11y_diagnostics()
            );
        }
    }

    #[test]
    fn window_a11y_epoch_is_the_bridge_handle_of_that_window() {
        let (_runtime, mut app, window, _label_signal, _label) = fixture();
        let other = app.create_window(WindowOptions::default());
        let first = app.window_a11y_epoch(window).expect("窗口存在");
        let again = app.window_a11y_epoch(window).expect("窗口存在");
        assert!(
            first.same_handle(&again),
            "同一窗口多次取句柄必须是同一份 Arc"
        );
        let bridge = app.window(window).expect("window").a11y_consumer_epoch();
        assert!(
            first.same_handle(&bridge),
            "必须与窗口 A11yBridge 的 consumer_epoch 同一份（平台回调 bump 才能被引擎看到）"
        );
        let other_epoch = app.window_a11y_epoch(other).expect("第二个窗口存在");
        assert!(
            !first.same_handle(&other_epoch),
            "不同窗口必须有独立的消费端代际"
        );
        assert!(
            app.window_a11y_epoch(WindowId::for_test(999)).is_none(),
            "不存在的窗口返回 None（平台建窗路径据此跳过）"
        );
    }
}
