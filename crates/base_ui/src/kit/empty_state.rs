//! 空态说明（旧 `crate::empty_state` 的引擎化重建，P2-4a）。
//!
//! 旧实现的语义原样保留：一句「为什么这里什么都没有」的弱化文字。它**不**画背景 / 边框 / 圆角
//! （它站在一个已经有这些的面上），**不**自己定尺寸（区域自己决定占多高），只做三件事：
//! 弱化色（`fg.muted`）、14 px 字号、左右 16 px 内边距与 `max_w_full` 防溢出。
//!
//! # a11y
//!
//! 它说的是「这里没有内容」，是一个普通文本（引擎按文本叶子给 `Label`），因此根容器不声明角色、
//! 不隐藏；读屏器会把这句话当作普通文字播报。
//!
//! # 与旧 API 的差异
//!
//! 旧 API 是 `EmptyState::new(ElementId, text)`；新 API 的 `key` 只用于调试 id，文本在
//! [`EmptyState::text`]。

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, tree::NodeTree},
    paint::style::StylePatch,
    text::TextStyle,
};

use crate::{
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};

/// 空态文字的字号（旧 `text_sm` = 0.875 rem = 14 px）。
const TEXT_SIZE: f32 = 14.0;

/// 空态说明。
#[derive(Clone)]
pub struct EmptyState {
    theme: ThemeHandle,
    key: SmolStr,
    text: SmolStr,
}

impl EmptyState {
    /// 建空态（文本随后用 [`EmptyState::text`]）。
    pub fn new(theme: ThemeHandle, key: impl Into<SmolStr>) -> Self {
        Self {
            theme,
            key: key.into(),
            text: SmolStr::default(),
        }
    }

    /// 说明文字。
    #[must_use]
    pub fn text(mut self, text: impl Into<SmolStr>) -> Self {
        self.text = text.into();
        self
    }

    /// 建节点（容器 + 文本叶子）。
    ///
    /// # Errors
    /// `StaleNode`（父节点陈旧）。
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
        style.inner.max_size.width = taffy::style::LengthPercentageAuto::percent(1.0);
        let root = NodeBuilder::boxed()
            .id(format_smolstr!("{}:empty", self.key))
            .style(style)
            .build(tree, parent)?;
        let theme = self.theme.clone();
        NodeBuilder::text(self.text.clone(), TextStyle::new(TEXT_SIZE))
            .bind_style(move |cx: &mut ReadCtx<'_>| StylePatch {
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

    /// 文本、字号与弱化色。
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

    /// 不画背景 / 边框 / 圆角：容器的动态补丁为空（只有布局与内边距）。
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

    /// a11y：文字叶子是 Label。
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
}
