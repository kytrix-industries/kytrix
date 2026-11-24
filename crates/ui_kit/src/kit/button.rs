use std::{cell::RefCell, rc::Rc};

use smol_str::SmolStr;
use taffy::style::{Dimension, JustifyContent};
use ui_core::{
    app::App,
    error::EngineError,
    event::{Action, Event},
    interaction::ActionCtx,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId},
    paint::style::StylePatch,
    text::TextStyle,
    window::WindowId,
};

use crate::{
    kit::{
        icon::IconKind,
        interactive::{Interactive, RolePalette, StateRoles, mount_dynamic_icon},
        join::joined_edges,
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{components::InteractiveVariant, role::Role},
};

const CORNER_RADIUS: LogicalPx = LogicalPx::new(4.0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ButtonVariant {
    #[default]
    Primary,
    Secondary,
    Outline,
    Ghost,
    Danger,
}

#[derive(Clone)]
pub struct Button {
    interactive: Interactive,
    variant: ButtonVariant,
    size: UnifiedSize,
    icon: Option<IconKind>,
    icon_size: Option<LogicalPx>,
    label: Option<SmolStr>,
    a11y_label: Option<SmolStr>,
    disabled: bool,
    loading: bool,
    selected: bool,
    action: Option<Action>,
    callback: Option<Rc<dyn Fn(&mut ActionCtx<'_>)>>,
    tooltip: Option<SmolStr>,
    joined: Option<(usize, usize)>,
    geometry: Rc<RefCell<Option<NodeId>>>,
}

impl Button {
    pub fn new(
        app: &mut App,
        theme: crate::theme::handle::ThemeHandle,
        key: impl Into<SmolStr>,
    ) -> Self {
        Self {
            interactive: Interactive::new(app, theme, key),
            variant: ButtonVariant::Primary,
            size: UnifiedSize::Medium,
            icon: None,
            icon_size: None,
            label: None,
            a11y_label: None,
            disabled: false,
            loading: false,
            selected: false,
            action: None,
            callback: None,
            tooltip: None,
            joined: None,
            geometry: Rc::new(RefCell::new(None)),
        }
    }

    #[must_use]
    pub fn primary(mut self) -> Self {
        self.variant = ButtonVariant::Primary;
        self
    }

    #[must_use]
    pub fn secondary(mut self) -> Self {
        self.variant = ButtonVariant::Secondary;
        self
    }

    #[must_use]
    pub fn outline(mut self) -> Self {
        self.variant = ButtonVariant::Outline;
        self
    }

    #[must_use]
    pub fn ghost(mut self) -> Self {
        self.variant = ButtonVariant::Ghost;
        self
    }

    #[must_use]
    pub fn danger(mut self) -> Self {
        self.variant = ButtonVariant::Danger;
        self
    }

    #[must_use]
    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    #[must_use]
    pub fn icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    #[must_use]
    pub fn icon_size(mut self, size: LogicalPx) -> Self {
        self.icon_size = Some(size);
        self
    }

    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn a11y_label(mut self, label: impl Into<SmolStr>) -> Self {
        self.a11y_label = Some(label.into());
        self
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    #[must_use]
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    #[must_use]
    pub fn action(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    #[must_use]
    pub fn on_click(mut self, callback: impl Fn(&mut ActionCtx<'_>) + 'static) -> Self {
        self.callback = Some(Rc::new(callback));
        self
    }

    #[must_use]
    pub fn tooltip_text(mut self, text: impl Into<SmolStr>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    #[must_use]
    pub fn joined(mut self, index: usize, count: usize) -> Self {
        self.joined = Some((index, count));
        self
    }

    #[must_use]
    pub fn node(&self) -> Option<NodeId> {
        *self.geometry.borrow()
    }

    #[must_use]
    pub fn label_text(&self) -> Option<&str> {
        self.label.as_deref()
    }

    #[must_use]
    pub fn a11y_label_text(&self) -> Option<&str> {
        self.a11y_label.as_deref().or(self.label.as_deref())
    }

    #[must_use]
    pub fn tooltip(&self) -> Option<&str> {
        self.tooltip.as_deref()
    }

    #[must_use]
    pub fn interactive(&self) -> &Interactive {
        &self.interactive
    }

    #[must_use]
    pub fn action_identity(&self) -> Option<Action> {
        self.action.clone().or_else(|| {
            self.callback
                .as_ref()
                .map(|_| Action::Custom(smol_str::format_smolstr!("{}.click", self.key())))
        })
    }

    #[must_use]
    pub fn key(&self) -> &str {
        self.interactive.key()
    }

    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let mut interactive = self.interactive.clone().with_unified_size(self.size);
        if let Some(label) = self.a11y_label.as_ref().or(self.label.as_ref()) {
            interactive = interactive.label(label.clone());
        }
        if let Some(action) = &self.action {
            interactive = interactive.action(action.clone());
        }
        if let Some(tooltip) = &self.tooltip {
            interactive = interactive.tooltip_text(tooltip.clone());
        }
        interactive = match self.variant {
            ButtonVariant::Primary => interactive.variant(InteractiveVariant::Filled),
            ButtonVariant::Secondary => interactive.variant(InteractiveVariant::Tonal),
            ButtonVariant::Outline => interactive.variant(InteractiveVariant::Outline),
            ButtonVariant::Ghost => interactive.variant(InteractiveVariant::Ghost),
            ButtonVariant::Danger => interactive.role_palette(danger_palette()),
        };
        interactive = interactive
            .disabled(self.disabled)
            .loading(self.loading)
            .selected(self.selected);
        if let Some((index, count)) = self.joined {
            let radii = joined_edges(CORNER_RADIUS, px(1.0), index, count).corner_radius;
            interactive = interactive.corner_radii(radii);
        }
        if let Some(callback) = &self.callback {
            let callback = callback.clone();
            interactive = interactive.on_click(move |ctx| callback(ctx));
        }
        let label = self.label.clone();
        let icon = self.icon;
        let size = self.size;
        let icon_size = self.icon_size.unwrap_or_else(|| size.icon_size());
        let for_icon = interactive.clone();
        let for_text = interactive.clone();
        let watcher = interactive.clone();
        let node = interactive.build(app, window, parent, move |app, window, parent| {
            if let Some(kind) = icon {
                let icon_node =
                    mount_dynamic_icon(app, window, parent, kind, icon_size, move |cx| {
                        StylePatch {
                            foreground: Some(for_icon.foreground_color(cx)),
                            ..StylePatch::default()
                        }
                    })?;
                watcher.watch(icon_node);
            }
            if let Some(text) = label {
                let tree = app
                    .window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut();
                let label_node = NodeBuilder::text(text, TextStyle::new(size.text_size()))
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(for_text.foreground_color(cx)),
                        ..StylePatch::default()
                    })
                    .build(tree, parent)?;
                watcher.watch(label_node);
            }
            Ok(())
        })?;
        *self.geometry.borrow_mut() = Some(node);
        Ok(node)
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        self.interactive.sync(app)
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.interactive.dispatch(app, event)
    }

    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        self.interactive.set_disabled(app, disabled)
    }

    pub fn set_loading(&self, app: &mut App, loading: bool) -> bool {
        self.interactive.set_loading(app, loading)
    }

    pub fn set_selected(&self, app: &mut App, selected: bool) -> bool {
        self.interactive.set_selected(app, selected)
    }

    pub fn release(&self, app: &mut App) -> usize {
        self.interactive.release(app)
    }
}

impl UnifiedSizable for Button {
    fn with_unified_size(mut self, size: UnifiedSize) -> Self {
        self.size = size;
        self
    }
}

pub(crate) fn stretch_button(
    app: &mut App,
    window: WindowId,
    node: NodeId,
) -> Result<(), EngineError> {
    let tree = app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    let mut style = tree.get(node)?.style().clone();
    style
        .flex(1.0, 1.0, Dimension::length(0.0))
        .justify_content(JustifyContent::CENTER);
    tree.set_style(node, style)
}

fn danger_palette() -> RolePalette {
    RolePalette {
        normal: StateRoles {
            background: Some(Role::StatusDangerBase),
            foreground: Role::FgOnBrand,
            border: Some(Role::StatusDangerBase),
        },
        hover: StateRoles {
            background: Some(Role::StatusDangerBase),
            foreground: Role::FgOnBrand,
            border: Some(Role::StatusDangerBorder),
        },
        active: StateRoles {
            background: Some(Role::StatusDangerBorder),
            foreground: Role::FgOnBrand,
            border: Some(Role::StatusDangerBorder),
        },
        disabled: StateRoles {
            background: Some(Role::SurfaceControl),
            foreground: Role::FgDisabled,
            border: Some(Role::SurfaceControl),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use ui_core::{
        a11y::A11yRole,
        event::{ActionSource, InputEvent, Modifiers, PointerButton},
        math::{LogicalPoint, px},
        node::{id::NodeId, tree::NodeKind},
        paint::color::{Color, ColorResolver},
        test_support::app::TestApp,
    };

    use super::{Button, ButtonVariant};
    use crate::{
        kit::{icon::IconKind, size::UnifiedSizable},
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

    struct Harness {
        app: TestApp,
        button: Button,
        node: NodeId,
    }

    fn harness(configure: impl FnOnce(Button) -> Button) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let button = configure(Button::new(app.app_mut(), theme, "button").label("Save"));
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = button.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let _ = button.sync(app.app_mut());
        let _ = app.drain().expect("frame");
        Harness { app, button, node }
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

    fn resolved(color: Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    fn light_theme() -> Theme {
        Theme::of(Appearance::Light)
    }

    #[test]
    fn every_button_variant_maps_to_its_theme_recipe() {
        let components = light_theme().components().interactive;
        let cases = [
            (ButtonVariant::Primary, components.filled.normal, "primary"),
            (
                ButtonVariant::Secondary,
                components.tonal.normal,
                "secondary",
            ),
            (ButtonVariant::Outline, components.outline.normal, "outline"),
            (ButtonVariant::Ghost, components.ghost.normal, "ghost"),
        ];
        for (variant, recipe, name) in cases {
            let harness = harness(|button| button.variant(variant));
            assert_eq!(
                patch(&harness).background.map(resolved),
                Some(resolved(recipe.background)),
                "{name} 的背景"
            );
            assert_eq!(
                patch(&harness).foreground.map(resolved),
                Some(resolved(recipe.foreground)),
                "{name} 的前景"
            );
        }
        let harness = harness(|button| button.danger());
        let theme = light_theme();
        assert_eq!(
            patch(&harness).background.map(resolved),
            Some(resolved(theme.role(Role::StatusDangerBase))),
            "danger 的背景是状态危险色"
        );
        assert_eq!(
            patch(&harness).foreground.map(resolved),
            Some(resolved(theme.role(Role::FgOnBrand))),
            "danger 的文字是 on-brand 色"
        );
    }

    #[test]
    fn a_button_with_an_icon_draws_the_icon_before_the_label() {
        let harness = harness(|button| button.icon(IconKind::Check).large());
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        let children = tree.get(harness.node).expect("node").children();
        assert_eq!(children.len(), 2, "图标 + 文字");
        assert!(matches!(
            tree.get(children[0]).expect("icon").kind(),
            NodeKind::Image(_)
        ));
        match tree.get(children[1]).expect("label").kind() {
            NodeKind::Text(content) => {
                assert_eq!(content.text, "Save");
                assert_eq!(
                    content.style.size,
                    crate::kit::size::UnifiedSize::Large.text_size()
                );
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn the_size_ladder_reaches_layout_text_and_icon() {
        let harness = harness(|button| button.icon(IconKind::Check).large());
        let bounds = harness.app.probe().bounds(harness.node).expect("bounds");
        assert_eq!(bounds.size.height, px(40.0), "大号按钮高 40");
        let window = harness.app.window_id();
        let tree = harness.app.app().window(window).expect("window").tree();
        let children = tree.get(harness.node).expect("node").children();
        let icon_style = tree.get(children[0]).expect("icon").style();
        assert_eq!(
            icon_style.inner().size.width,
            taffy::style::Dimension::length(16.0),
            "大号图标边长 16"
        );
        let label_style = tree.get(children[1]).expect("label").style();
        assert_eq!(
            label_style.inner().size.height,
            taffy::style::Dimension::auto(),
            "文字节点高度由行高决定"
        );
    }

    #[test]
    fn an_icon_size_override_restores_the_legacy_12px_icon() {
        fn icon_width(configure: impl FnOnce(Button) -> Button) -> f32 {
            let harness = harness(configure);
            let window = harness.app.window_id();
            let tree = harness.app.app().window(window).expect("window").tree();
            let children = tree.get(harness.node).expect("node").children();
            tree.get(children[0])
                .expect("icon")
                .style()
                .inner()
                .size
                .width
                .into_option()
                .expect("图标有显式尺寸")
        }

        assert_eq!(
            icon_width(|button| button.icon(IconKind::ArrowsClockwise).small()),
            8.0,
            "small 档的图标 token 是 8 px（旧 size_2）"
        );
        assert_eq!(
            icon_width(|button| button
                .icon(IconKind::ArrowsClockwise)
                .small()
                .icon_size(px(12.0))),
            12.0,
            "显式覆盖恢复旧 size_3 的 12 px"
        );
    }

    #[test]
    fn a_button_reads_its_configured_labels_back() {
        let harness = harness(|button| button.a11y_label("Close").tooltip_text("Close dialog"));
        assert_eq!(harness.button.label_text(), Some("Save"), "可见文字读回");
        assert_eq!(
            harness.button.a11y_label_text(),
            Some("Close"),
            "显式可访问名优先于可见文字"
        );
        assert_eq!(harness.button.tooltip(), Some("Close dialog"));
        assert_eq!(
            harness.button.label_text(),
            Some("Save"),
            "build 之后仍可读回（配置值不被 build 消费）"
        );

        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let bare = Button::new(app.app_mut(), theme.clone(), "bare");
        assert_eq!(bare.label_text(), None);
        assert_eq!(bare.a11y_label_text(), None);
        assert_eq!(bare.tooltip(), None);
        let labeled = Button::new(app.app_mut(), theme, "labeled").label("OK");
        assert_eq!(
            labeled.a11y_label_text(),
            Some("OK"),
            "没有显式 a11y 名时回落到可见文字"
        );
    }

    #[test]
    fn a_disabled_button_does_not_invoke_its_callback() {
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let mut harness = harness(move |button| {
            button.disabled(true).on_click(move |_| {
                counter.set(counter.get() + 1);
            })
        });
        assert_eq!(patch(&harness).interactive, Some(false));
        let point = LogicalPoint::new(
            harness
                .app
                .probe()
                .bounds(harness.node)
                .expect("bounds")
                .origin
                .x
                + 10.0,
            harness
                .app
                .probe()
                .bounds(harness.node)
                .expect("bounds")
                .origin
                .y
                + 10.0,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert_eq!(calls.get(), 0, "禁用按钮不得触发 on_click");
    }

    #[test]
    fn a_loading_button_is_busy_and_not_interactive() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let button = Button::new(app.app_mut(), theme, "loading")
            .label("Save")
            .loading(true);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = button.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert_eq!(record.role, A11yRole::Button);
        assert!(record.busy, "加载中的按钮必须宣告 busy");
        assert!(record.disabled, "加载中的按钮不可交互");
    }

    #[test]
    fn a_button_declares_the_button_role_and_its_label() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let button = Button::new(app.app_mut(), theme, "a11y").label("Save");
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = button.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("Save"));
    }

    #[test]
    fn an_icon_only_button_reports_its_a11y_label() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let button = Button::new(app.app_mut(), theme, "icon-only")
            .icon(IconKind::X)
            .a11y_label("Close");
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let node = button.build(app.app_mut(), window, root).expect("build");
        let _ = app.drain().expect("frame");
        let snapshot = app.a11y().expect("a11y");
        let record = snapshot.node_for(node).expect("a11y node");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(
            record.label_text(),
            Some("Close"),
            "icon-only 按钮必须有可访问名"
        );
    }

    #[test]
    fn a_click_runs_the_callback_and_a_disabled_programmatic_action_does_not() {
        let calls = Rc::new(Cell::new(0usize));
        let counter = calls.clone();
        let mut harness = harness(move |button| {
            button.on_click(move |_| {
                counter.set(counter.get() + 1);
            })
        });
        let window = harness.app.window_id();
        let bounds = harness.app.probe().bounds(harness.node).expect("bounds");
        let point = LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = harness.app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = harness.app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert_eq!(calls.get(), 1, "指针点击触发 on_click");

        assert!(harness.button.set_disabled(harness.app.app_mut(), true));
        let action = harness.button.action_identity().expect("自动动作");
        let _ = harness.app.app_mut().dispatch_action(
            window,
            harness.node,
            action,
            ActionSource::Programmatic,
        );
        assert_eq!(calls.get(), 1, "禁用后程序化 click 不再触发");
    }

    #[test]
    fn joined_buttons_square_their_shared_corners() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        let first = Button::new(app.app_mut(), theme.clone(), "join-0")
            .label("A")
            .joined(0, 2);
        let second = Button::new(app.app_mut(), theme, "join-1")
            .label("B")
            .joined(1, 2);
        first.build(app.app_mut(), window, root).expect("first");
        second.build(app.app_mut(), window, root).expect("second");
        let record = app.drain().expect("frame").clone();
        let frame = record.frame.as_ref().expect("rendered");
        let radii: Vec<[f32; 4]> = frame
            .quads
            .iter()
            .map(|quad| quad.corner_radius.map(|radius| radius.value()))
            .collect();
        assert!(
            radii.contains(&[4.0, 0.0, 0.0, 4.0]),
            "首按钮保留左外角：{radii:?}"
        );
        assert!(
            radii.contains(&[0.0, 4.0, 4.0, 0.0]),
            "尾按钮保留右外角：{radii:?}"
        );
    }
}
