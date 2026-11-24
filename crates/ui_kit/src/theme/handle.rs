use std::rc::Rc;

use ui_core::{
    app::App,
    node::{dependency::ReadCtx, signal::Signal},
    paint::color::{Color, ColorResolver, LinearPremulRgba},
};

use super::{
    appearance::Appearance,
    components::{ComponentColors, ComponentTokens},
    resolved::Theme,
    role::Role,
};

#[derive(Clone, Debug)]
pub struct ThemeHandle {
    inner: Rc<ThemeData>,
}

#[derive(Debug)]
struct ThemeData {
    current: Signal<Appearance>,
    light: Theme,
    dark: Theme,
}

impl ThemeData {
    fn theme(&self, appearance: Appearance) -> &Theme {
        match appearance {
            Appearance::Light => &self.light,
            Appearance::Dark => &self.dark,
        }
    }
}

impl ThemeHandle {
    pub fn new(app: &mut App, appearance: Appearance) -> Self {
        Self {
            inner: Rc::new(ThemeData {
                current: app.signal(appearance),
                light: Theme::of(Appearance::Light),
                dark: Theme::of(Appearance::Dark),
            }),
        }
    }

    pub fn set_appearance(&self, app: &mut App, appearance: Appearance) -> bool {
        app.write(&self.inner.current, appearance)
    }

    pub fn appearance(&self, cx: &mut ReadCtx<'_>) -> Appearance {
        *cx.read(&self.inner.current)
    }

    pub fn current(&self, app: &App) -> Appearance {
        app.read_with(&self.inner.current, |appearance| *appearance)
    }

    pub fn theme(&self, cx: &mut ReadCtx<'_>) -> &Theme {
        let appearance = self.appearance(cx);

        self.inner.theme(appearance)
    }

    pub fn role(&self, cx: &mut ReadCtx<'_>, role: Role) -> Color {
        self.theme(cx).role(role)
    }

    pub fn resolve(&self, cx: &mut ReadCtx<'_>, role: Role) -> LinearPremulRgba {
        ColorResolver.resolve(self.role(cx, role))
    }

    pub fn theme_for(&self, appearance: Appearance) -> &Theme {
        self.inner.theme(appearance)
    }

    pub fn components(&self, cx: &mut ReadCtx<'_>) -> &ComponentColors {
        self.theme(cx).components()
    }

    pub fn tokens(&self) -> &ComponentTokens {
        self.inner.light.tokens()
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{
        math::{LogicalRect, px},
        node::builder::NodeBuilder,
        paint::{
            color::{Color, ColorResolver},
            style::{PaintSpec, StylePatch},
        },
        test_support::app::TestApp,
    };

    use super::ThemeHandle;
    use crate::theme::{appearance::Appearance, resolved::Theme, role::Role};

    #[test]
    fn the_handle_starts_in_the_appearance_it_was_given() {
        let mut app = TestApp::new();
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);

        assert_eq!(
            handle.theme_for(Appearance::Light).appearance(),
            Appearance::Light
        );
        assert_eq!(
            handle.theme_for(Appearance::Dark).appearance(),
            Appearance::Dark
        );
        assert_eq!(handle.tokens(), Theme::of(Appearance::Light).tokens());
    }

    #[test]
    fn switching_to_the_same_appearance_short_circuits() {
        let mut app = TestApp::new();
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let _ = app.drain().expect("首帧").clone();

        assert!(!handle.set_appearance(app.app_mut(), Appearance::Light));
        let record = app.drain().expect("同外观").clone();
        assert!(!record.rendered, "同外观不写信号、不失效、不出帧");
    }

    #[test]
    fn switching_appearance_invalidates_only_the_nodes_that_read_the_theme() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let other = app.app_mut().signal(0u8);
        let root = app.app().window(window).expect("窗口").tree().root();

        let theme_reader = {
            let theme = handle.clone();
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0)))
                .paint(PaintSpec {
                    background: Color::WHITE,
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| StylePatch {
                    background: Some(theme.role(cx, Role::SurfaceCanvas)),
                    ..StylePatch::default()
                })
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("主题读者")
        };
        let appearance_reader = {
            let theme = handle.clone();
            NodeBuilder::text("light", ui_core::text::TextStyle::new(px(13.0)))
                .bind_text(move |cx| match theme.appearance(cx) {
                    Appearance::Light => smol_str::SmolStr::new_static("light"),
                    Appearance::Dark => smol_str::SmolStr::new_static("night"),
                })
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("外观读者")
        };
        let static_node = NodeBuilder::boxed()
            .layout_override(LogicalRect::new(px(0.0), px(30.0), px(20.0), px(10.0)))
            .paint(PaintSpec {
                background: Color::WHITE,
                ..PaintSpec::default()
            })
            .build(
                app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                root,
            )
            .expect("静态节点");
        let other_reader = {
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(50.0), px(20.0), px(10.0)))
                .paint(PaintSpec {
                    background: Color::WHITE,
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| StylePatch {
                    opacity: Some(f32::from(*cx.read(&other)) / 255.0),
                    ..StylePatch::default()
                })
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("其它信号读者")
        };

        let first = app.drain().expect("首帧").clone();
        assert!(first.rendered);

        assert!(handle.set_appearance(app.app_mut(), Appearance::Dark));
        let second = app.drain().expect("切深色").clone();
        assert!(second.rendered, "写外观必须出一帧");

        assert!(
            second.dirty_nodes.contains(&theme_reader),
            "读过主题角色的节点必须失效：{:?}",
            second.dirty_nodes
        );
        assert!(
            second.dirty_nodes.contains(&appearance_reader),
            "读过外观信号的文本节点必须失效：{:?}",
            second.dirty_nodes
        );
        assert!(
            !second.dirty_nodes.contains(&static_node),
            "静态节点不得被主题切换失效：{:?}",
            second.dirty_nodes
        );
        assert!(
            !second.dirty_nodes.contains(&other_reader),
            "只读其它信号的节点不得被主题切换失效：{:?}",
            second.dirty_nodes
        );

        let expected = ColorResolver
            .resolve(Theme::of(Appearance::Dark).role(Role::SurfaceCanvas))
            .components;
        let quad = second
            .frame
            .as_ref()
            .expect("已提交")
            .quads
            .iter()
            .find(|quad| quad.node == theme_reader)
            .expect("主题读者的图元");
        assert_eq!(quad.fill.components, expected);
        assert_ne!(
            quad.fill.components,
            ColorResolver
                .resolve(Theme::of(Appearance::Light).role(Role::SurfaceCanvas))
                .components,
            "深色画布色与浅色不同"
        );

        let static_quad = second
            .frame
            .as_ref()
            .expect("已提交")
            .quads
            .iter()
            .find(|quad| quad.node == static_node)
            .expect("静态图元仍在场景里");
        assert_eq!(
            static_quad.fill.components,
            ColorResolver.resolve(Color::WHITE).components
        );
    }

    #[test]
    fn a_reader_with_constant_output_is_still_invalidated() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let root = app.app().window(window).expect("窗口").tree().root();
        let constant = Color::WHITE;

        let reader = {
            let theme = handle.clone();
            NodeBuilder::boxed()
                .layout_override(LogicalRect::new(px(0.0), px(0.0), px(20.0), px(20.0)))
                .paint(PaintSpec {
                    background: constant,
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| {
                    let _ = theme.appearance(cx);

                    StylePatch {
                        background: Some(constant),
                        ..StylePatch::default()
                    }
                })
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("恒定输出读者")
        };
        let _ = app.drain().expect("首帧").clone();

        assert!(handle.set_appearance(app.app_mut(), Appearance::Dark));
        let record = app.drain().expect("切深色").clone();

        assert!(
            record.dirty_nodes.contains(&reader),
            "读过主题的节点在写信号时就被标脏，与输出是否变化无关：{:?}",
            record.dirty_nodes
        );
        let quad = record
            .frame
            .as_ref()
            .expect("已提交")
            .quads
            .iter()
            .find(|quad| quad.node == reader)
            .expect("恒定输出读者的图元");
        assert_eq!(
            quad.fill.components,
            ColorResolver.resolve(constant).components,
            "输出不变：画出的填充仍是同一个值"
        );
    }

    #[test]
    fn one_write_invalidates_every_node_that_read_any_role() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let handle = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let root = app.app().window(window).expect("窗口").tree().root();

        let mut nodes = Vec::with_capacity(2);
        for (role, index) in [(Role::SurfaceCanvas, 0.0_f32), (Role::FgMuted, 20.0)] {
            let theme = handle.clone();
            nodes.push(
                NodeBuilder::boxed()
                    .layout_override(LogicalRect::new(px(0.0), px(index), px(10.0), px(10.0)))
                    .paint(PaintSpec {
                        background: Color::WHITE,
                        ..PaintSpec::default()
                    })
                    .bind_style(move |cx| StylePatch {
                        background: Some(theme.role(cx, role)),
                        ..StylePatch::default()
                    })
                    .build(
                        app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                        root,
                    )
                    .expect("节点"),
            );
        }
        let _ = app.drain().expect("首帧").clone();

        assert!(handle.set_appearance(app.app_mut(), Appearance::Dark));
        let record = app.drain().expect("切深色").clone();

        for node in &nodes {
            assert!(
                record.dirty_nodes.contains(node),
                "节点 {node:?} 读过主题但没被失效：{:?}",
                record.dirty_nodes
            );
        }
    }
}
