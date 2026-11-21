//! 分割线（旧 `crate::separator` 的引擎化重建，P2-4a）。
//!
//! 旧实现是一条 1 px 的 div：横向占满宽度、纵向 `self_stretch`，颜色取 `line.divider`。新实现
//! 是同样的布局，只是颜色走 `bind_style`（主题切换跟着变）。
//!
//! # a11y
//!
//! 引擎的 `A11yRole` 没有 `Separator`（AccessKit 有，但 §7.4 的映射表没有把它列为引擎角色），
//! 分割线是纯装饰：显式 `hidden`，不产生 `MissingAltText` 诊断，也不占用可访问名（登记见
//! spec-delta §4）。

use ui_core::{
    a11y::A11yOverride,
    node::{builder::NodeBuilder, id::NodeId, tree::NodeTree},
    paint::style::StylePatch,
};

use crate::{
    style_builder::StyleBuilder,
    theme::{handle::ThemeHandle, role::Role},
};

/// 分割线方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeparatorOrientation {
    /// 横向：1 px 高、占满宽度。
    Horizontal,
    /// 纵向：1 px 宽、拉伸到父容器高度。
    Vertical,
}

/// 分割线。
#[derive(Clone)]
pub struct Separator {
    theme: ThemeHandle,
    orientation: SeparatorOrientation,
}

impl Separator {
    /// 横向分割线。
    pub fn horizontal(theme: ThemeHandle) -> Self {
        Self {
            theme,
            orientation: SeparatorOrientation::Horizontal,
        }
    }

    /// 纵向分割线。
    pub fn vertical(theme: ThemeHandle) -> Self {
        Self {
            theme,
            orientation: SeparatorOrientation::Vertical,
        }
    }

    /// 方向。
    #[must_use]
    pub fn orientation(&self) -> SeparatorOrientation {
        self.orientation
    }

    /// 建分割线节点。
    ///
    /// # Errors
    /// `StaleNode`（父节点陈旧）。
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
                style.inner.align_self = Some(taffy::style::AlignSelf::STRETCH);
                style
            }
        };
        // 分割线不参与伸缩（旧 `.flex_shrink_0()`）。
        style.inner.flex_shrink = 0.0;
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
        paint::color::ColorResolver,
        test_support::app::TestApp,
    };

    use super::{Separator, SeparatorOrientation};
    use crate::{
        style_builder::StyleBuilder,
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

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

    /// 横向：1 px 高、占满父宽度；纵向：1 px 宽、拉伸到父高度。
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

    /// 颜色是 `line.divider`；a11y 显式隐藏（装饰）。
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
