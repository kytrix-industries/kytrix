use ui_core::{
    a11y::A11yOverride,
    node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    paint::{style::StylePatch, style_builder::StyleBuilder},
};

use crate::theme::{handle::ThemeHandle, role::Role};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeparatorOrientation {
    Horizontal,
    Vertical,
}

#[derive(Clone)]
pub struct Separator {
    theme: ThemeHandle,
    orientation: SeparatorOrientation,
}

impl Separator {
    pub fn horizontal(theme: ThemeHandle) -> Self {
        Self {
            theme,
            orientation: SeparatorOrientation::Horizontal,
        }
    }

    pub fn vertical(theme: ThemeHandle) -> Self {
        Self {
            theme,
            orientation: SeparatorOrientation::Vertical,
        }
    }

    #[must_use]
    pub fn orientation(&self) -> SeparatorOrientation {
        self.orientation
    }

    pub fn build(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
    ) -> Result<NodeId, ui_core::error::EngineError> {
        let mut style = match self.orientation {
            SeparatorOrientation::Horizontal => {
                StyleBuilder::new().width_full().height(1.0).build()
            }
            SeparatorOrientation::Vertical => {
                let mut style = StyleBuilder::new().width(1.0).build();
                style.align_self(taffy::style::AlignSelf::STRETCH);
                style
            }
        };
        style.flex_shrink(0.0);
        let theme = self.theme.clone();
        NodeBuilder::boxed()
            .style(style)
            .bind_style(move |cx| StylePatch {
                background: Some(theme.role(cx, Role::LineDivider)),
                ..StylePatch::default()
            })
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, parent)
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{
        node::{builder::NodeBuilder, id::NodeId},
        paint::{color::ColorResolver, style_builder::StyleBuilder},
        test_support::app::TestApp,
    };

    use super::{Separator, SeparatorOrientation};
    use crate::theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role};

    fn build(
        orientation: SeparatorOrientation,
        parent_size: (f32, f32),
    ) -> (TestApp, NodeId, Theme) {
        let mut app = TestApp::new();
        let theme = Theme::of(Appearance::Light);
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let separator = match orientation {
            SeparatorOrientation::Horizontal => Separator::horizontal(handle),
            SeparatorOrientation::Vertical => Separator::vertical(handle),
        };
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let parent = {
            let tree = app.app_mut().window_mut(window).expect("window").tree_mut();
            NodeBuilder::boxed()
                .style(
                    StyleBuilder::new()
                        .width(parent_size.0)
                        .height(parent_size.1)
                        .build(),
                )
                .build(tree, root)
                .expect("parent")
        };
        let node = separator
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                parent,
            )
            .expect("build");
        let _ = app.drain().expect("frame");
        (app, node, theme)
    }

    #[test]
    fn horizontal_and_vertical_separators_take_one_pixel_on_their_axis() {
        let (app, node, _) = build(SeparatorOrientation::Horizontal, (200.0, 100.0));
        let bounds = app.probe().bounds(node).expect("bounds");
        assert_eq!(bounds.size.height, 1.0);
        assert!((bounds.size.width - 200.0).abs() < 0.5);
        let (app, node, _) = build(SeparatorOrientation::Vertical, (200.0, 100.0));
        let bounds = app.probe().bounds(node).expect("bounds");
        assert_eq!(bounds.size.width, 1.0);
        assert!(
            (bounds.size.height - 100.0).abs() < 0.5,
            "纵向分割线拉伸到父高"
        );
    }

    #[test]
    fn the_separator_uses_the_divider_role_and_is_decorative() {
        let (app, node, theme) = build(SeparatorOrientation::Horizontal, (200.0, 100.0));
        let window = app.window_id();
        let patch = app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("node")
            .style_patch()
            .background
            .expect("背景");
        assert_eq!(
            ColorResolver.resolve(patch).components,
            ColorResolver
                .resolve(theme.role(Role::LineDivider))
                .components
        );
        let mut app = app;
        app.enable_a11y_capture();
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert!(record.hidden, "分割线是装饰性节点");
    }
}
