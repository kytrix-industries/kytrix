//! 应用层主题服务：一个 `Signal<Appearance>` + 两套已加载的 [`Theme`]。
//!
//! 这是旧 `style/theme.rs` 的 gpui `Global` + `refresh_windows()`（整窗重绘）的引擎化替代：主题版本
//! 是依赖图里的一等信号，`role(&mut ReadCtx, ..)` 读它并登记依赖，因此运行时切换深浅色**只失效读过
//! 主题的节点**（设计 §3.4 / §4.9.4 的 I6/I17 验收面）。没有读主题的节点（静态绘制、只读其它信号
//! 的节点）不会被写信号触达，也不会产生一帧。
//!
//! **失效语义（引擎事实，不是"值变才重绘"）**：写信号时 `ui_core` 的 `invalidate` 直接给每个视觉
//! 读者标 `BIND` + `PAINT`（`StylePatch::affects_layout` 时是 `BIND` + `LAYOUT`），不等
//! `bind_style`/`bind_text` 求值后的差分。求值只决定补丁/文本是否真的替换；一个始终返回同一值的
//! 主题读者在切换外观后**仍会**被标脏并进帧（`a_reader_with_constant_output_is_still_invalidated`
//! 记录了这一点）。"输出不变则不重绘"若成为产品要求，属于引擎 P2-0/P2-4 的差分优化，不在本批。
//!
//! 两套 [`Theme`] 在构造时各解析一次；切换只写信号、不重算主题。句柄 `Clone` 是 `Rc` 浅拷贝（每个
//! `bind_style`/`bind_text` 闭包持有一份），所以节点侧的 clone 不复制两套主题数据。
//!
//! 组件取色的三条路：
//!
//! - **动态**：`bind_style(move |cx| StylePatch { background: Some(theme.role(cx, Role::SurfaceCanvas)), .. })`
//!   ——读主题信号，深浅切换自动失效（文本 / 图片调制共用 `foreground` 通道）；
//! - **帧外重算**：`theme.current(app)` 取当前外观，组件在 `sync` 里重取颜色（自绘画布）或重建
//!   静态载荷子树（`MarkdownNode`）；节点侧再用一条只读外观的空 `bind_style` 登记依赖，保证
//!   「写外观 → 标脏 → 出一帧 → `sync`」的闭环；
//! - **挂载期常量**：`theme.theme_for(appearance)` / `Theme::of(..)` —— 只适用于与外观无关的
//!   几何/文案；**颜色不得走这条路**（否则浅色主题下会残留深色文字，见 `kit/mod.rs` 的 G3 一节）。

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

/// 应用层主题服务（`Rc` 句柄，可自由 clone 进 `bind_*` 闭包）。
#[derive(Clone, Debug)]
pub struct ThemeHandle {
    inner: Rc<ThemeData>,
}

/// 句柄共享的数据：主题信号 + 两套已加载主题。
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
    /// 建主题服务：写一个 `Signal<Appearance>` 初值，并解析两种外观的主题。
    ///
    /// 调用方（组合根）之后用 [`ThemeHandle::set_appearance`] 推进外观；设置层决定用哪种外观。
    pub fn new(app: &mut App, appearance: Appearance) -> Self {
        Self {
            inner: Rc::new(ThemeData {
                current: app.signal(appearance),
                light: Theme::of(Appearance::Light),
                dark: Theme::of(Appearance::Dark),
            }),
        }
    }

    /// 把应用画成 `appearance`。
    ///
    /// 返回 `true` 表示外观真的变了：主题信号被写入，读过它的节点已标脏；`false` 表示外观没变
    /// （信号等值短路），既不失效也不产生帧。这是旧 `apply` 的"同外观不重绘"语义。
    pub fn set_appearance(&self, app: &mut App, appearance: Appearance) -> bool {
        app.write(&self.inner.current, appearance)
    }

    /// 读当前外观，并在 `cx` 所属节点上登记主题依赖。
    pub fn appearance(&self, cx: &mut ReadCtx<'_>) -> Appearance {
        *cx.read(&self.inner.current)
    }

    /// 读当前外观，**不登记依赖**：帧外 `sync` 的「外观是否变了」判据（帧钩子里比较后按需重算
    /// 静态载荷；节点侧的失效由 `bind_*` 里的 [`ThemeHandle::appearance`] 负责）。
    pub fn current(&self, app: &App) -> Appearance {
        app.read_with(&self.inner.current, |appearance| *appearance)
    }

    /// 读当前外观对应的主题（登记依赖）。
    pub fn theme(&self, cx: &mut ReadCtx<'_>) -> &Theme {
        let appearance = self.appearance(cx);

        self.inner.theme(appearance)
    }

    /// 读一个语义角色的颜色（登记依赖）：深浅切换只失效读过它的节点。
    pub fn role(&self, cx: &mut ReadCtx<'_>, role: Role) -> Color {
        self.theme(cx).role(role)
    }

    /// 读一个角色的线性预乘 RGBA（登记依赖；挂载期或动态补丁都可用的出口）。
    pub fn resolve(&self, cx: &mut ReadCtx<'_>, role: Role) -> LinearPremulRgba {
        ColorResolver.resolve(self.role(cx, role))
    }

    /// 某个外观的主题，**不读信号、不登记依赖**：挂载期静态常量与调色板审计用。
    pub fn theme_for(&self, appearance: Appearance) -> &Theme {
        self.inner.theme(appearance)
    }

    /// 组件颜色配方（登记依赖）。
    pub fn components(&self, cx: &mut ReadCtx<'_>) -> &ComponentColors {
        self.theme(cx).components()
    }

    /// 组件度量 token（与外观无关，不登记依赖）。
    pub fn tokens(&self) -> &ComponentTokens {
        self.inner.light.tokens()
    }
}

#[cfg(test)]
mod tests {
    use ui_core::{
        math::Rect,
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

    /// I6/I17 的验收：深浅色切换只失效**读过主题**的节点。
    ///
    /// 三个节点：①`bind_style` 读主题角色；②`bind_text` 读当前外观；③静态绘制、④只读另一个信号。
    /// 写外观后，①②被重绘（①②的补丁/文本真的变了），③④不被触达。
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
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
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
            NodeBuilder::text("light", ui_core::text::TextStyle::new(13.0))
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
            .layout_override(Rect::new(0.0, 30.0, 20.0, 10.0))
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
                .layout_override(Rect::new(0.0, 50.0, 20.0, 10.0))
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

        // 主题读者真的画出了深色外观的画布色（不是只标脏）。
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

        // 静态节点的图元原样保留（没有被重画，也没有被主题改写）。
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

    /// 输出恒定的主题读者在切换外观后**仍会**被标脏：引擎在写信号时就标 `PAINT`，不做输出差分。
    ///
    /// 这是审查 F2-1（P2）要求固定的真实行为：不是"值变才重绘"，而是"读过就重算"。这里断言的
    /// 是节点的失效状态与帧的产出；画出的像素不变（补丁等值）。
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
                .layout_override(Rect::new(0.0, 0.0, 20.0, 20.0))
                .paint(PaintSpec {
                    background: constant,
                    ..PaintSpec::default()
                })
                .bind_style(move |cx| {
                    // 读主题 → 登记依赖，但输出与外观无关（永远同一个值）。
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

    /// 两个节点共享同一份主题依赖：一次写入把两者都失效（主题是一个信号，不是每个颜色一个）。
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
                    .layout_override(Rect::new(0.0, index, 10.0, 10.0))
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
