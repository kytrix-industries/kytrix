use smallvec::{SmallVec, smallvec};

use super::{App, StylePassReport};
use crate::{
    event::{Action, KeyBinding, KeyCode, Modifiers},
    interaction::ActionCtx,
    node::{
        dependency::{Owner, ReadCtx, ReadTracker},
        dirty::DirtyFlags,
        signal::SignalId,
    },
    paint::style::StylePatch,
    text::{TextContent, TextStyle},
    window::WindowId,
};

pub(crate) fn on_action(
    app: &mut App,
    action: Action,
    handler: impl FnMut(&mut ActionCtx<'_>) + 'static,
) {
    app.handlers.insert(action, Box::new(handler));
}

pub(crate) fn off_action(app: &mut App, action: &Action) -> bool {
    app.handlers.remove(action).is_some()
}

#[cfg(feature = "test-support")]
pub(crate) fn has_action_handler(app: &App, action: &Action) -> bool {
    app.handlers.contains_key(action)
}

#[cfg(feature = "test-support")]
pub(crate) fn scoped_binding_count(
    app: &App,
    window: WindowId,
    scope: crate::node::id::NodeId,
) -> usize {
    app.scoped_bindings
        .iter()
        .filter(|binding| binding.window == window && binding.scope == scope)
        .count()
}

pub(crate) fn bind_key(app: &mut App, binding: KeyBinding) {
    app.bindings.retain(|existing| {
        !(existing.key == binding.key && existing.modifiers == binding.modifiers)
    });
    app.bindings.push(binding);
}

pub(crate) fn bind_key_scoped(
    app: &mut App,
    window: crate::window::WindowId,
    scope: crate::node::id::NodeId,
    binding: KeyBinding,
) -> Result<(), crate::error::EngineError> {
    use crate::error::EngineError;
    let Some(target) = app.windows.get(&window) else {
        return Err(EngineError::UnknownWindow { window });
    };
    target.tree().get(scope)?;
    app.scoped_bindings.retain(|existing| {
        !(existing.window == window
            && existing.scope == scope
            && existing.binding.key == binding.key
            && existing.binding.modifiers == binding.modifiers)
    });
    app.scoped_bindings.push(super::ScopedKeyBinding {
        window,
        scope,
        binding,
    });
    Ok(())
}

pub(crate) fn unbind_key_scoped(
    app: &mut App,
    window: crate::window::WindowId,
    scope: crate::node::id::NodeId,
    key: KeyCode,
    modifiers: Modifiers,
) -> bool {
    let before = app.scoped_bindings.len();
    app.scoped_bindings.retain(|existing| {
        !(existing.window == window
            && existing.scope == scope
            && existing.binding.key == key
            && existing.binding.modifiers == modifiers)
    });
    app.scoped_bindings.len() != before
}

pub(crate) fn unbind_scope_keys(
    app: &mut App,
    window: crate::window::WindowId,
    scope: crate::node::id::NodeId,
) -> usize {
    let before = app.scoped_bindings.len();
    app.scoped_bindings
        .retain(|existing| !(existing.window == window && existing.scope == scope));
    before - app.scoped_bindings.len()
}

pub(crate) fn unbind_key(app: &mut App, key: KeyCode, modifiers: Modifiers) -> bool {
    let before = app.bindings.len();
    app.bindings
        .retain(|binding| !(binding.key == key && binding.modifiers == modifiers));
    app.bindings.len() != before
}

pub(crate) fn refresh_styles(app: &mut App, window: WindowId) -> Option<StylePassReport> {
    let App {
        store,
        graph,
        windows,
        text,
        eval_stack,
        ..
    } = app;
    let window_state = windows.get_mut(&window)?;
    let tree = window_state.tree_mut();
    debug_assert!(eval_stack.is_empty(), "上一帧的派生求值栈泄漏");
    eval_stack.clear();

    let mut evaluated = 0usize;
    let mut text_evaluated = 0usize;
    let mut text_changed = 0usize;
    let nodes = tree.take_dirty_bind();
    for node in nodes.iter().copied() {
        let needs_eval = tree
            .get(node)
            .is_ok_and(|n| n.dirty().contains(DirtyFlags::BIND));
        if !needs_eval {
            tree.clear_bind(node);
            continue;
        }
        let old_patch = tree
            .get(node)
            .map_or_else(|_| StylePatch::default(), |n| n.style_patch().clone());
        let compute = tree.take_bind_style(node);
        let bind_text = tree.take_bind_text(node);
        let bind_a11y = tree.take_bind_a11y(node);
        if compute.is_none() && bind_text.is_none() && bind_a11y.is_none() {
            let ghost_deps = tree.deps(node).unwrap_or(&[]);
            graph.replace_deps(Owner::Node { window, node }, ghost_deps, &[]);
            tree.set_deps(node, SmallVec::new());
            tree.clear_bind(node);
            continue;
        }

        let (values, derived) = store.split();
        let mut tracker = ReadTracker::new();
        let (patch, text_value, a11y_value, owner) = {
            let mut ctx = ReadCtx::new(
                values,
                derived,
                graph,
                &mut tracker,
                &mut *eval_stack,
                Owner::Node { window, node },
            );
            let patch = compute
                .as_ref()
                .map_or_else(StylePatch::default, |compute| compute(&mut ctx));
            let text_value = bind_text.as_ref().map(|bind| bind(&mut ctx));
            let a11y_value = bind_a11y
                .as_ref()
                .map_or_else(crate::a11y::A11yPatch::default, |bind| bind(&mut ctx));
            (patch, text_value, a11y_value, ctx.owner())
        };
        if compute.is_some() {
            evaluated += 1;
        }
        if text_value.is_some() {
            text_evaluated += 1;
        }

        let old_deps = tree.deps(node).unwrap_or(&[]);
        graph.replace_deps(owner, old_deps, tracker.as_slice());
        tree.set_deps(node, tracker.into_inner());

        if let Some(compute) = compute {
            let layout_changed = patch.affects_layout || old_patch.affects_layout;
            let a11y_changed =
                patch.interactive != old_patch.interactive || patch.visible != old_patch.visible;
            tree.set_style_patch(node, patch);
            tree.put_bind_style(node, compute);
            if layout_changed {
                tree.mark_layout(node);
            }
            if a11y_changed {
                tree.mark_subtree_a11y(node);
            }
        }
        let mut text_changed_here = false;
        if let Some(bind) = bind_text {
            tree.put_bind_text(node, bind);
        }
        if let Some(value) = text_value {
            let style = tree
                .text_content(node)
                .map_or_else(TextStyle::default, |content| content.style.clone());
            let changed = tree
                .text_content(node)
                .is_none_or(|content| content.text != value);
            if changed {
                let mut content = tree
                    .text_content(node)
                    .cloned()
                    .unwrap_or_else(|| TextContent::new(value.clone(), style.clone()));
                content.text = value;
                content.style = style;
                let clipped_runs = content.clip_runs_to_text();
                if clipped_runs > 0 {
                    text.push_diagnostics(vec![crate::text::TextDiagnostic::TextRunClipped {
                        runs: clipped_runs,
                    }]);
                }
                tree.set_text_content(node, content);
                text_changed += 1;
                text_changed_here = true;
            }
        }
        if let Some(bind) = bind_a11y {
            let _ = crate::a11y::tree::write_a11y_patch(tree, node, a11y_value);
            tree.put_bind_a11y(node, bind);
        }
        tree.clear_bind(node);
        if text_changed_here {
            tree.mark_paint(node);
            tree.mark_a11y(node);
        }
    }
    tree.recycle(nodes);

    let layout_dirty = tree.has_layout_work();
    let paint_dirty = tree.has_paint_work();
    Some(StylePassReport {
        evaluated,
        layout_dirty,
        paint_dirty,
        wake_requested: app.wake_requested,
        text_evaluated,
        text_changed,
    })
}

pub(crate) fn invalidate(app: &mut App, signal: SignalId) {
    let App {
        store,
        graph,
        windows,
        ..
    } = app;
    let mut worklist: SmallVec<[SignalId; 8]> = smallvec![signal];
    let mut marked = 0usize;

    while let Some(current) = worklist.pop() {
        let Some(owners) = graph.dependents_mut(current) else {
            continue;
        };
        owners.retain(|owner| match owner {
            Owner::Node { window, node } => windows
                .get(window)
                .is_some_and(|w| w.tree().get(*node).is_ok()),
            Owner::Derived(_) => true,
        });
        let empty = owners.is_empty();
        let snapshot: SmallVec<[Owner; 8]> = owners.iter().copied().collect();
        if empty {
            graph.remove_signal(current);
            continue;
        }

        for owner in snapshot {
            match owner {
                Owner::Node { window, node } => {
                    let Some(target) = windows.get_mut(&window) else {
                        continue;
                    };
                    let a11y_only =
                        target.tree().has_a11y_bind(node) && !target.tree().has_visual_bind(node);
                    let affects_layout = target
                        .tree()
                        .get(node)
                        .is_ok_and(|n| n.style_patch().affects_layout);
                    let alive = if a11y_only {
                        let queued = target.a11y_note_binding(node);
                        let marked = target.tree_mut().mark_a11y(node);
                        queued || marked
                    } else if affects_layout {
                        target.tree_mut().mark_bind(node);
                        target.tree_mut().mark_layout(node)
                    } else {
                        target.tree_mut().mark_bind(node);
                        target.tree_mut().mark_paint(node)
                    };
                    if alive {
                        marked += 1;
                    }
                }
                Owner::Derived(derived) => {
                    if store.mark_derived_dirty(derived) {
                        worklist.push(store.derived_handle(derived));
                    }
                }
            }
        }
    }

    if marked > 0 {
        app.request_frame();
    }
}

#[cfg(test)]
mod tests {
    use smol_str::SmolStr;

    use super::{App, refresh_styles};
    use crate::{
        math::px,
        node::builder::NodeBuilder,
        text::{InlineAlign, InlineBoxSpec, TextRun, TextStyle},
        window::WindowOptions,
    };

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let app = App::new(runtime.handle().clone());
        (runtime, app)
    }

    #[test]
    fn bind_text_preserves_runs() {
        use crate::paint::color::Color;

        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal = app.signal(SmolStr::new_static("abcdef"));
        let bold = TextStyle::new(px(16.0)).with_weight(700);
        let red = TextStyle::new(px(16.0)).with_color(Color::WHITE);
        let node = {
            let w = app.window_mut(window).expect("window");
            let root = w.tree().root();
            NodeBuilder::text("abcdef", TextStyle::new(px(16.0)))
                .runs([
                    TextRun::new(0..2, bold.clone()),
                    TextRun::new(2..6, red.clone()),
                ])
                .bind_text(move |cx| cx.read(&signal).clone())
                .build(w.tree_mut(), root)
                .expect("build")
        };
        let _ = refresh_styles(&mut app, window);
        let content = app
            .window(window)
            .expect("window")
            .tree()
            .text_content(node)
            .expect("text")
            .clone();
        assert_eq!(content.runs().len(), 2, "runs 在 bind_text 后必须保留");
        assert_eq!(content.runs()[0].range, 0..2);
        assert_eq!(content.runs()[0].style, bold);
        assert_eq!(content.runs()[1].range, 2..6);
        assert_eq!(content.runs()[1].style, red);
    }

    #[test]
    fn bind_text_crops_runs_to_the_new_text_and_diagnoses() {
        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal = app.signal(SmolStr::new_static("abcdef"));
        let bold = TextStyle::new(px(16.0)).with_weight(700);
        let node = {
            let w = app.window_mut(window).expect("window");
            let root = w.tree().root();
            NodeBuilder::text("abcdef", TextStyle::new(px(16.0)))
                .runs([TextRun::new(0..2, bold.clone()), TextRun::new(2..6, bold)])
                .bind_text(move |cx| cx.read(&signal).clone())
                .build(w.tree_mut(), root)
                .expect("build")
        };
        let _ = refresh_styles(&mut app, window);
        app.write(&signal, SmolStr::new_static("abc"));
        let _ = refresh_styles(&mut app, window);
        let content = app
            .window(window)
            .expect("window")
            .tree()
            .text_content(node)
            .expect("text")
            .clone();
        assert_eq!(content.text, "abc");
        assert_eq!(content.runs().len(), 2, "裁剪保留 run（只内收区间）");
        assert_eq!(content.runs()[1].range, 2..3, "越界端点按新文本内收");
        assert!(
            app.text_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                crate::text::TextDiagnostic::TextRunClipped { runs } if *runs == 1
            )),
            "裁剪必须可观测：{:?}",
            app.text_diagnostics()
        );
    }

    #[test]
    fn bind_text_preserves_runs_and_inline_boxes_together() {
        let (_runtime, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let signal = app.signal(SmolStr::new_static("a b"));
        let spec = InlineBoxSpec {
            id: 9,
            index: 2,
            size: crate::math::LogicalSize::new(px(8.0), px(8.0)),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        };
        let node = {
            let w = app.window_mut(window).expect("window");
            let root = w.tree().root();
            NodeBuilder::text("a b", TextStyle::new(px(16.0)))
                .runs([TextRun::new(
                    0..1,
                    TextStyle::new(px(16.0)).with_weight(700),
                )])
                .inline_boxes([spec])
                .bind_text(move |cx| cx.read(&signal).clone())
                .build(w.tree_mut(), root)
                .expect("build")
        };
        let _ = refresh_styles(&mut app, window);
        let content = app
            .window(window)
            .expect("window")
            .tree()
            .text_content(node)
            .expect("text")
            .clone();
        assert_eq!(content.text, "a b");
        assert_eq!(content.runs().len(), 1);
        assert_eq!(content.inline_boxes.len(), 1);
    }
}
