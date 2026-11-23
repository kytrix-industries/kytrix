use std::{thread, time::Duration};

use ui_core::{
    a11y::{A11yNode, A11yNodeId, A11yTreeSnapshot},
    app::App,
    math::{Rect, Scale},
    node::id::NodeId,
    paint::primitive::{Image, Primitive},
    test_support::{
        a11y::{ShadowTree, tree_update_snapshot},
        app::TestApp,
        record::FrameRecord,
    },
    window::{WindowId, WindowOptions},
};

#[must_use]
pub fn test_app() -> TestApp {
    TestApp::new()
}

#[must_use]
pub fn test_app_with_scale(scale: f32) -> TestApp {
    let scale = Scale::new(scale).expect("测试 DPI 必须是有限正数");
    TestApp::with_options(WindowOptions {
        scale,
        ..WindowOptions::default()
    })
}

#[must_use]
pub fn root(app: &TestApp) -> NodeId {
    app.app()
        .window(app.window_id())
        .expect("测试窗口必须存在")
        .tree()
        .root()
}

pub fn frame(app: &mut TestApp) -> FrameRecord {
    app.drain().expect("测试帧").clone()
}

#[must_use]
pub fn image_primitives(app: &TestApp) -> Vec<Image> {
    app.app()
        .window(app.window_id())
        .expect("测试窗口必须存在")
        .scene()
        .iter()
        .filter_map(|primitive| match primitive {
            Primitive::Image(image) => Some(*image),
            _ => None,
        })
        .collect()
}

#[must_use]
pub fn bounds(app: &TestApp, node: NodeId) -> Option<Rect> {
    app.probe().bounds(node)
}

#[must_use]
pub fn overlay_wrapper(app: &App, window: WindowId, panel: NodeId) -> NodeId {
    let tree = app.window(window).expect("测试窗口必须存在").tree();
    let root = tree.root();
    let mut cursor = panel;
    while let Some(parent) = tree.get(cursor).ok().and_then(|node| node.parent()) {
        if parent == root {
            return cursor;
        }
        cursor = parent;
    }
    panic!("浮层面板不在 root 子树下：{panel:?}");
}

pub fn assert_single_image(app: &TestApp) -> Image {
    let images = image_primitives(app);
    assert_eq!(images.len(), 1, "期望恰好一张图片，实际：{images:?}");
    images[0]
}

pub struct DecodeApp {
    _runtime: tokio::runtime::Runtime,
    app: App,
    window: WindowId,
}

impl DecodeApp {
    #[must_use]
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("异步解码测试的 runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions::default());
        Self {
            _runtime: runtime,
            app,
            window,
        }
    }

    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    #[must_use]
    pub fn app(&self) -> &App {
        &self.app
    }

    #[must_use]
    pub fn window_id(&self) -> WindowId {
        self.window
    }

    pub fn poll_until<T>(
        &mut self,
        attempts: usize,
        mut probe: impl FnMut(&mut App) -> Option<T>,
    ) -> Option<T> {
        for _ in 0..attempts {
            let _ = self.app.frame(self.window).expect("解码宿主的帧");
            if let Some(value) = probe(&mut self.app) {
                return Some(value);
            }
            thread::sleep(Duration::from_millis(2));
        }
        probe(&mut self.app)
    }
}

impl Default for DecodeApp {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Default)]
pub struct A11yUpdates {
    shadow: ShadowTree,
    applied: usize,
}

impl A11yUpdates {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, app: &TestApp) -> bool {
        let Some(update) = app.a11y_update() else {
            return false;
        };
        if let Err(error) = self.shadow.apply(update) {
            panic!(
                "TreeUpdate 违反 AccessKit 契约：{error}\n--- update ---\n{}",
                tree_update_snapshot(update)
            );
        }
        self.applied += 1;
        true
    }

    #[must_use]
    pub fn applied(&self) -> usize {
        self.applied
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.shadow.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shadow.is_empty()
    }
}

#[must_use]
pub fn full_a11y(app: &mut TestApp) -> A11yTreeSnapshot {
    if let Some(diff) = app.a11y_oracle_diff() {
        panic!("a11y 增量与全量分叉：{diff}");
    }
    app.assert_a11y_contract();
    app.a11y().expect("强制全树交付必须产出 a11y 快照").clone()
}

#[must_use]
pub fn a11y_snapshot_without_oracle(app: &mut TestApp) -> A11yTreeSnapshot {
    let _ = app.a11y_oracle_diff();
    app.assert_a11y_contract();
    app.a11y().expect("强制全树交付必须产出 a11y 快照").clone()
}

pub fn frame_a11y(app: &mut TestApp) {
    let _ = app.drain().expect("a11y 帧");
    app.assert_a11y_contract();
}

#[must_use]
pub fn a11y_of(snapshot: &A11yTreeSnapshot, node: NodeId) -> &A11yNode {
    snapshot
        .node_for(node)
        .unwrap_or_else(|| panic!("a11y 快照里没有引擎节点 {node:?}（需要全树快照）"))
}

#[must_use]
pub fn a11y_id(snapshot: &A11yTreeSnapshot, node: NodeId) -> A11yNodeId {
    a11y_of(snapshot, node).id
}

pub fn assert_single_parent_per_node(snapshot: &A11yTreeSnapshot) {
    let mut by_id: ahash::HashMap<A11yNodeId, &A11yNode> = ahash::HashMap::default();
    for node in snapshot.nodes() {
        by_id.insert(node.id, node);
    }
    let mut parent_of: ahash::HashMap<A11yNodeId, A11yNodeId> = ahash::HashMap::default();
    for node in snapshot.nodes() {
        for child in &node.children {
            let Some(child_node) = by_id.get(child) else {
                panic!(
                    "节点 {:?} 的 children 里有快照外的子节点 {child:?}（不可达 / 孤儿）",
                    node.engine
                );
            };
            if let Some(previous) = parent_of.insert(*child, node.id) {
                assert_eq!(
                    previous, node.id,
                    "节点 {:?} 同时挂在 {previous:?} 与 {:?} 两个父下",
                    child_node.engine, node.id
                );
            }
        }
    }
    for node in snapshot.nodes() {
        let Some(parent) = node.parent else {
            continue;
        };
        let Some(parent_node) = by_id.get(&parent) else {
            continue;
        };
        assert!(
            parent_node.children.contains(&node.id),
            "节点 {:?} 的父 {parent:?} 没有把它列进 children（孤儿）",
            node.engine
        );
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{math::Rect, node::builder::NodeBuilder, resources::DecodeState};

    use super::{
        DecodeApp, assert_single_image, bounds, frame, image_primitives, root, test_app,
        test_app_with_scale,
    };

    #[test]
    fn the_default_harness_is_a_live_window_with_an_empty_tree() {
        let mut app = test_app();
        let window_root = root(&app);
        let record = frame(&mut app);
        assert!(record.frame_index.get() >= 1, "首帧必须有提交序号");
        assert_eq!(root(&app), window_root, "帧不重建根节点");
        assert!(image_primitives(&app).is_empty(), "空树没有图片图元");
        assert!(bounds(&app, window_root).is_some(), "根节点必须有布局盒");
    }

    #[test]
    fn the_probe_reads_layout_boxes() {
        let mut app = test_app();
        let window = app.window_id();
        let rect = Rect::new(4.0, 6.0, 20.0, 12.0);
        let window_root = root(&app);
        let node = NodeBuilder::boxed()
            .layout_override(rect)
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                window_root,
            )
            .expect("挂载");
        let _ = app.drain().expect("首帧");
        assert_eq!(bounds(&app, node), Some(rect));
    }

    #[test]
    fn assert_single_image_counts() {
        let mut app = test_app();
        let window = app.window_id();
        let window_root = root(&app);
        for _ in 0..2 {
            let _ = crate::kit::icon::IconSpec::new(crate::kit::icon::IconKind::Check)
                .build(app.app_mut(), window, window_root)
                .expect("图标");
        }
        let _ = app.drain().expect("首帧");
        assert_eq!(image_primitives(&app).len(), 2);
        let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_single_image(&app);
        }));
        assert!(failure.is_err(), "两张图时 assert_single_image 必须失败");
    }

    #[test]
    fn the_scale_factory_sets_the_window_scale() {
        let hdpi = test_app_with_scale(2.0);
        assert_eq!(
            hdpi.app()
                .window(hdpi.window_id())
                .expect("窗口")
                .scale()
                .factor(),
            2.0
        );
        let normal = test_app();
        assert_eq!(
            normal
                .app()
                .window(normal.window_id())
                .expect("窗口")
                .scale()
                .factor(),
            1.0
        );
    }

    #[test]
    fn the_decode_host_delivers_async_results() {
        let mut host = DecodeApp::new();
        let bytes = bytes::Bytes::copy_from_slice(
            crate::kit::icon::svg(crate::kit::icon::IconKind::User).as_bytes(),
        );
        let pending = host
            .app_mut()
            .resources_mut()
            .insert_svg_async_at_size(bytes, ui_core::math::Size::new(48.0, 48.0));
        let delivered = host.poll_until(600, |app| {
            let results = app.take_resource_results();
            results
                .into_iter()
                .find(|(handle, _)| *handle == pending)
                .map(|(_, state)| state)
        });
        let Some(state) = delivered else {
            panic!("异步解码必须在 ≈1.2 s 内交付");
        };
        match state {
            DecodeState::Ready(image) => {
                assert_eq!((image.width, image.height), (48, 48));
            }
            other => panic!("图标 SVG 应栅格成功，实际 {other:?}"),
        }
    }
}
