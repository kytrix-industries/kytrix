use smol_str::SmolStr;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    math::px,
    node::{builder::NodeBuilder, dependency::ReadCtx, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{
        color::Color,
        primitive::Border,
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::TextStyle,
};

use crate::{
    kit::{
        size::{UnifiedSizable, UnifiedSize},
        text_value::TextValue,
    },
    theme::{handle::ThemeHandle, role::Role},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BadgeColor {
    #[default]
    Neutral,
    Info,
    Success,
    Warning,
    Elevated,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeShape {
    Tonal,
    Solid,
    Outline,
}

#[derive(Clone)]
pub struct Badge {
    theme: ThemeHandle,
    color: BadgeColor,
    shape: Option<BadgeShape>,
    size: UnifiedSize,
    label: TextValue,
}

impl Badge {
    pub fn new(theme: ThemeHandle) -> Self {
        Self {
            theme,
            color: BadgeColor::default(),
            shape: None,
            size: UnifiedSize::Medium,
            label: TextValue::default(),
        }
    }

    #[must_use]
    pub fn text(mut self, text: impl Into<SmolStr>) -> Self {
        self.label = TextValue::Static(text.into());
        self
    }

    #[must_use]
    pub fn bind_label(mut self, label: Signal<SmolStr>) -> Self {
        self.label = TextValue::Bound(label);
        self
    }

    #[must_use]
    pub fn neutral(mut self) -> Self {
        self.color = BadgeColor::Neutral;
        self
    }

    #[must_use]
    pub fn info(mut self) -> Self {
        self.color = BadgeColor::Info;
        self
    }

    #[must_use]
    pub fn success(mut self) -> Self {
        self.color = BadgeColor::Success;
        self
    }

    #[must_use]
    pub fn warning(mut self) -> Self {
        self.color = BadgeColor::Warning;
        self
    }

    #[must_use]
    pub fn elevated(mut self) -> Self {
        self.color = BadgeColor::Elevated;
        self
    }

    #[must_use]
    pub fn error(mut self) -> Self {
        self.color = BadgeColor::Error;
        self
    }

    #[must_use]
    pub fn tonal(mut self) -> Self {
        self.shape = Some(BadgeShape::Tonal);
        self
    }

    #[must_use]
    pub fn solid(mut self) -> Self {
        self.shape = Some(BadgeShape::Solid);
        self
    }

    #[must_use]
    pub fn outline(mut self) -> Self {
        self.shape = Some(BadgeShape::Outline);
        self
    }

    #[must_use]
    pub fn shape(&self) -> BadgeShape {
        self.shape.unwrap_or(match self.color {
            BadgeColor::Neutral => BadgeShape::Solid,
            BadgeColor::Info
            | BadgeColor::Success
            | BadgeColor::Warning
            | BadgeColor::Elevated
            | BadgeColor::Error => BadgeShape::Tonal,
        })
    }

    pub fn build(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
    ) -> Result<NodeId, ui_core::error::EngineError> {
        let theme = self.theme.clone();
        let color = self.color;
        let shape = self.shape();
        let size = self.size;
        let label = self.label.clone();
        let initial = label.initial_text();
        let mut root = NodeBuilder::boxed()
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .justify_center()
                    .gap(size.gap())
                    .padding_x(size.px())
                    .height(size.h())
                    .border(px(1.0))
                    .build(),
            )
            .paint(PaintBuilder::new().rounded(px(4.0)).build())
            .bind_style(move |cx| {
                let (background, foreground, border) = badge_colors(cx, &theme, color, shape);
                StylePatch {
                    background: Some(background),
                    foreground: Some(foreground),
                    border: Some(Border {
                        width: px(1.0),
                        color: border,
                    }),
                    ..StylePatch::default()
                }
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::Label),
                label: (!label.is_bound()).then(|| initial.clone()),
                ..A11yOverride::default()
            });
        if label.is_bound() {
            let bound = label.clone();
            root = root.bind_a11y(move |cx| A11yPatch {
                label: Some(bound.read(cx)),
                ..A11yPatch::default()
            });
        }
        let root = root.build(tree, parent)?;
        let theme_text = self.theme.clone();
        let mut text = NodeBuilder::text(initial, TextStyle::new(size.text_size()));
        if label.is_bound() {
            text = text.bind_text(move |cx| label.read(cx));
        }
        text.bind_style(move |cx| {
            let (_, foreground, _) = badge_colors(cx, &theme_text, color, shape);
            StylePatch {
                foreground: Some(foreground),
                ..StylePatch::default()
            }
        })
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, root)?;
        Ok(root)
    }
}

impl UnifiedSizable for Badge {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.size = size;
        self
    }
}

fn badge_colors(
    cx: &mut ReadCtx<'_>,
    theme: &ThemeHandle,
    color: BadgeColor,
    shape: BadgeShape,
) -> (Color, Color, Color) {
    let transparent = Color::TRANSPARENT;
    if color == BadgeColor::Neutral {
        return match shape {
            BadgeShape::Tonal => (
                theme.role(cx, Role::SurfaceControl),
                theme.role(cx, Role::FgDefault),
                theme.role(cx, Role::LineBorder),
            ),
            BadgeShape::Solid => (
                theme.role(cx, Role::SurfaceInverse),
                theme.role(cx, Role::FgOnInverse),
                theme.role(cx, Role::SurfaceInverse),
            ),
            BadgeShape::Outline => (
                transparent,
                theme.role(cx, Role::FgDefault),
                theme.role(cx, Role::LineDivider),
            ),
        };
    }
    let (base, surface, border) = status_roles(color);
    match shape {
        BadgeShape::Tonal => (
            theme.role(cx, surface),
            theme.role(cx, base),
            theme.role(cx, border),
        ),
        BadgeShape::Solid => (
            theme.role(cx, base),
            theme.role(cx, Role::FgOnBrand),
            theme.role(cx, base),
        ),
        BadgeShape::Outline => (transparent, theme.role(cx, base), theme.role(cx, border)),
    }
}

fn status_roles(color: BadgeColor) -> (Role, Role, Role) {
    match color {
        BadgeColor::Neutral => (Role::FgDefault, Role::SurfaceControl, Role::LineBorder),
        BadgeColor::Info => (
            Role::StatusInfoBase,
            Role::StatusInfoSurface,
            Role::StatusInfoBorder,
        ),
        BadgeColor::Success => (
            Role::StatusSuccessBase,
            Role::StatusSuccessSurface,
            Role::StatusSuccessBorder,
        ),
        BadgeColor::Warning => (
            Role::StatusWarningBase,
            Role::StatusWarningSurface,
            Role::StatusWarningBorder,
        ),
        BadgeColor::Elevated => (
            Role::StatusElevatedBase,
            Role::StatusElevatedSurface,
            Role::StatusElevatedBorder,
        ),
        BadgeColor::Error => (
            Role::StatusDangerBase,
            Role::StatusDangerSurface,
            Role::StatusDangerBorder,
        ),
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{
        a11y::A11yRole,
        math::px,
        node::{id::NodeId, tree::NodeKind},
        paint::color::ColorResolver,
        test_support::app::TestApp,
    };

    use super::{Badge, BadgeColor, BadgeShape};
    use crate::{
        kit::size::UnifiedSizable,
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

    struct Harness {
        app: TestApp,
        node: NodeId,
        theme: Theme,
    }

    fn harness(configure: impl FnOnce(Badge) -> Badge) -> Harness {
        let mut app = TestApp::new();
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let theme = Theme::of(Appearance::Light);
        let badge = configure(Badge::new(handle));
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = badge
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("build");
        let _ = app.drain().expect("frame");
        Harness { app, node, theme }
    }

    fn patch(harness: &Harness) -> ui_core::paint::style::StylePatch {
        let window = harness.app.window_id();
        harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(harness.node)
            .expect("node")
            .style_patch()
            .clone()
    }

    fn resolved(color: ui_core::paint::color::Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    #[test]
    fn the_default_shape_depends_on_the_color() {
        let neutral = harness(|badge| badge.text("3"));
        assert_eq!(
            patch(&neutral).background.map(resolved),
            Some(resolved(neutral.theme.role(Role::SurfaceInverse))),
            "中性徽标默认是页面反色块"
        );
        let info = harness(|badge| badge.info().text("Info"));
        assert_eq!(
            patch(&info).background.map(resolved),
            Some(resolved(info.theme.role(Role::StatusInfoSurface))),
            "状态徽标默认是状态浅底"
        );
        assert_eq!(
            patch(&info).foreground.map(resolved),
            Some(resolved(info.theme.role(Role::StatusInfoBase)))
        );
    }

    #[test]
    fn explicit_shapes_pick_their_roles() {
        let outline = harness(|badge| badge.success().outline().text("OK"));
        assert_eq!(
            patch(&outline).background,
            Some(ui_core::paint::color::Color::TRANSPARENT)
        );
        assert_eq!(
            patch(&outline).foreground.map(resolved),
            Some(resolved(outline.theme.role(Role::StatusSuccessBase)))
        );
        assert_eq!(
            patch(&outline).border.map(|border| resolved(border.color)),
            Some(resolved(outline.theme.role(Role::StatusSuccessBorder)))
        );
        let solid = harness(|badge| badge.error().solid().text("Fail"));
        assert_eq!(
            patch(&solid).background.map(resolved),
            Some(resolved(solid.theme.role(Role::StatusDangerBase)))
        );
        assert_eq!(
            patch(&solid).foreground.map(resolved),
            Some(resolved(solid.theme.role(Role::FgOnBrand)))
        );
    }

    #[test]
    fn shape_defaults_follow_the_color() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        assert_eq!(Badge::new(theme.clone()).shape(), BadgeShape::Solid);
        assert_eq!(Badge::new(theme.clone()).info().shape(), BadgeShape::Tonal);
        assert_eq!(
            Badge::new(theme).info().outline().shape(),
            BadgeShape::Outline
        );
        assert_eq!(BadgeColor::default(), BadgeColor::Neutral);
    }

    #[test]
    fn the_badge_draws_its_text_once_and_declares_a_label_role() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let badge = Badge::new(handle).warning().small().text("72%");
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = badge
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("build");
        let _ = app.drain().expect("frame");
        let tree = app.app().window(window).expect("window").tree();
        let children = tree.get(node).expect("node").children();
        assert_eq!(children.len(), 1);
        match tree.get(children[0]).expect("text").kind() {
            NodeKind::Text(content) => {
                assert_eq!(content.text, "72%");
                assert_eq!(
                    content.style.size,
                    crate::kit::size::UnifiedSize::Small.text_size()
                );
            }
            other => panic!("expected Text, got {other:?}"),
        }
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert_eq!(record.role, A11yRole::Label);
        assert_eq!(record.label_text(), Some("72%"));
        let text_record = snapshot.node_for(children[0]).expect("text a11y");
        assert!(text_record.hidden, "文字叶子隐藏，避免重复播报");
    }

    #[test]
    fn the_size_ladder_reaches_the_layout() {
        let harness = harness(|badge| badge.text("x").small());
        let bounds = harness.app.probe().bounds(harness.node).expect("bounds");
        assert_eq!(bounds.size.height, px(20.0));
        assert!(
            (bounds.size.width - 4.0 - 4.0).abs() > px(-1.0),
            "宽度至少是内边距之和（实际 {}）",
            bounds.size.width
        );
    }

    #[test]
    fn a_bound_label_switches_language_without_rebuilding_the_badge() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let language = app.app_mut().signal(smol_str::SmolStr::new_static("综合"));
        let badge = Badge::new(handle).neutral().bind_label(language);
        assert!(badge.label.is_bound(), "bind_label 走绑定形态");
        assert!(
            !Badge::new(ThemeHandle::new(app.app_mut(), Appearance::Light))
                .text("Static")
                .label
                .is_bound(),
            "text() 保持静态形态"
        );
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = badge
            .build(
                app.app_mut().window_mut(window).expect("window").tree_mut(),
                root,
            )
            .expect("build");
        let _ = app.drain().expect("frame");

        let text_node = {
            let tree = app.app().window(window).expect("window").tree();
            let children = tree.get(node).expect("node").children();
            assert_eq!(children.len(), 1, "标签只有一段文字");
            children[0]
        };
        assert_eq!(text_of(&app, text_node), "综合", "首帧就是信号真值");
        assert_eq!(
            app.a11y()
                .expect("a11y")
                .node_for(node)
                .expect("a11y node")
                .label_text(),
            Some("综合"),
            "可访问名同样来自信号"
        );

        app.app_mut()
            .write(&language, smol_str::SmolStr::new_static("General"));
        let _ = app.drain().expect("frame");
        let tree = app.app().window(window).expect("window").tree();
        assert_eq!(
            tree.get(node).expect("node").children().to_vec(),
            vec![text_node],
            "语言切换不得重建标签节点或文字叶子"
        );
        assert_eq!(text_of(&app, text_node), "General");
        assert_eq!(
            app.a11y()
                .expect("a11y")
                .node_for(node)
                .expect("a11y node")
                .label_text(),
            Some("General"),
            "可访问名随信号更新"
        );
    }

    fn text_of(app: &TestApp, node: NodeId) -> smol_str::SmolStr {
        let window = app.window_id();
        let tree = app.app().window(window).expect("window").tree();
        match tree.get(node).expect("node").kind() {
            NodeKind::Text(content) => content.text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }
}
