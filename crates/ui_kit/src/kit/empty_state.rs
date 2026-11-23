use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::TextStyle,
};

use crate::{
    kit::text_value::TextValue,
    theme::{handle::ThemeHandle, role::Role},
};

const TEXT_SIZE: f32 = 14.0;

#[derive(Clone)]
pub struct EmptyState {
    theme: ThemeHandle,
    key: SmolStr,
    text: TextValue,
}

impl EmptyState {
    pub fn new(theme: ThemeHandle, key: impl Into<SmolStr>) -> Self {
        Self {
            theme,
            key: key.into(),
            text: TextValue::default(),
        }
    }

    #[must_use]
    pub fn text(mut self, text: impl Into<SmolStr>) -> Self {
        self.text = TextValue::Static(text.into());
        self
    }

    #[must_use]
    pub fn bind_text(mut self, text: Signal<SmolStr>) -> Self {
        self.text = TextValue::Bound(text);
        self
    }

    pub fn build(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
    ) -> Result<NodeId, ui_core::error::EngineError> {
        let mut style = StyleBuilder::new()
            .flex_none()
            .min_width_0()
            .padding_x(16.0)
            .build();
        style.max_width(taffy::style::LengthPercentageAuto::percent(1.0));
        let root = NodeBuilder::boxed()
            .id(format_smolstr!("{}:empty", self.key))
            .style(style)
            .build(tree, parent)?;
        let theme = self.theme.clone();
        let mut text = NodeBuilder::text(self.text.initial_text(), TextStyle::new(TEXT_SIZE));
        if self.text.is_bound() {
            let label = self.text.clone();
            text = text.bind_text(move |cx| label.read(cx));
        }
        text.bind_style(move |cx: &mut ReadCtx<'_>| StylePatch {
            foreground: Some(theme.role(cx, Role::FgMuted)),
            ..StylePatch::default()
        })
        .build(tree, root)?;
        Ok(root)
    }
}

impl std::fmt::Debug for EmptyState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EmptyState")
            .field("key", &self.key)
            .field("text", &self.text)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{
        node::{id::NodeId, tree::NodeKind},
        paint::color::ColorResolver,
        test_support::app::TestApp,
    };

    use super::EmptyState;
    use crate::theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role};

    fn build(text: &str) -> (TestApp, NodeId) {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let empty = EmptyState::new(theme, "empty").text(text);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = empty
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("build");
        let _ = app.drain().expect("frame");
        (app, node)
    }

    #[test]
    fn the_empty_state_draws_muted_small_text() {
        let (app, node) = build("No trades yet");
        let window = app.window_id();
        let tree = app.app().window(window).expect("window").tree();
        let children = tree.get(node).expect("node").children();
        assert_eq!(children.len(), 1);
        match tree.get(children[0]).expect("text").kind() {
            NodeKind::Text(content) => {
                assert_eq!(content.text, "No trades yet");
                assert_eq!(content.style.size, 14.0);
            }
            other => panic!("expected Text, got {other:?}"),
        }
        let theme = Theme::of(Appearance::Light);
        let patch = tree
            .get(children[0])
            .expect("text")
            .style_patch()
            .foreground
            .expect("foreground");
        assert_eq!(
            ColorResolver.resolve(patch).components,
            ColorResolver.resolve(theme.role(Role::FgMuted)).components
        );
    }

    #[test]
    fn the_empty_state_draws_no_surface_of_its_own() {
        let (app, node) = build("Nothing here");
        let window = app.window_id();
        let patch = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("node")
            .style_patch()
            .clone();
        assert_eq!(patch, ui_core::paint::style::StylePatch::default());
        let bounds = app.probe().bounds(node).expect("bounds");
        assert!(bounds.size.width > 0.0, "宽度由内容决定，不写死");
        assert!(bounds.size.height > 0.0);
    }

    #[test]
    fn the_empty_state_text_is_a_label() {
        let (mut app, node) = build("No results");
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        let window = app.window_id();
        let children = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("node")
            .children()
            .to_vec();
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(children[0]).expect("text node in a11y");
        assert_eq!(record.role, ui_core::a11y::A11yRole::Label);
        assert_eq!(record.label_text(), Some("No results"));
    }

    #[test]
    fn a_bound_text_switches_language_without_rebuilding_the_empty_state() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let language = app
            .app_mut()
            .signal(smol_str::SmolStr::new_static("暂无数据"));
        let empty = EmptyState::new(theme, "empty").bind_text(language);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = empty
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("build");
        let _ = app.drain().expect("frame");
        let text_node = {
            let tree = app.app().window(window).expect("window").tree();
            let children = tree.get(node).expect("node").children();
            assert_eq!(children.len(), 1);
            children[0]
        };
        assert_eq!(text_of(&app, text_node), "暂无数据");
        assert_eq!(
            app.a11y()
                .expect("a11y")
                .node_for(text_node)
                .expect("text a11y")
                .label_text(),
            Some("暂无数据")
        );

        app.app_mut()
            .write(&language, smol_str::SmolStr::new_static("No data yet"));
        let _ = app.drain().expect("frame");
        let tree = app.app().window(window).expect("window").tree();
        assert_eq!(
            tree.get(node).expect("node").children().to_vec(),
            vec![text_node],
            "语言切换不得重建空态节点或文字叶子"
        );
        assert_eq!(text_of(&app, text_node), "No data yet");
        assert_eq!(
            app.a11y()
                .expect("a11y")
                .node_for(text_node)
                .expect("text a11y")
                .label_text(),
            Some("No data yet")
        );
    }

    fn text_of(app: &TestApp, node: NodeId) -> smol_str::SmolStr {
        let window = app.window_id();
        let tree = app.app().window(window).expect("window").tree();
        match tree.get(node).expect("node").kind() {
            NodeKind::Text(content) => content.text.clone(),
            other => panic!("expected Text, got {other:?}"),
        }
    }
}
