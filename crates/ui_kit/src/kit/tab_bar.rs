use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::LengthPercentageAuto;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, KeyBinding, KeyCode, Modifiers},
    interaction::{ActionCtx, AnimatedValue, AnimationSpec, Easing},
    layout::LayoutOverride,
    math::{LogicalPx, LogicalRect, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
    },
    paint::{
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    kit::{
        icon::IconKind,
        interactive::{Interactive, RolePalette, StateRoles, mount_dynamic_icon},
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{handle::ThemeHandle, role::Role},
};

const TRACK_RADIUS: f32 = 4.0;
const TRACK_PADDING: f32 = 4.0;
const TAB_GAP: f32 = 4.0;
const TAB_MAX_WIDTH: f32 = 160.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndicatorAnimation {
    #[default]
    Fade,
    Slide,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabItem {
    pub label: SmolStr,
}

impl TabItem {
    #[must_use]
    pub fn new(label: impl Into<SmolStr>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TabBarOptions {
    pub size: UnifiedSize,
    pub indicator: IndicatorAnimation,
    pub closable: bool,
}

impl Default for TabBarOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            indicator: IndicatorAnimation::Fade,
            closable: false,
        }
    }
}

#[derive(Clone)]
pub struct TabBar {
    inner: Rc<TabBarInner>,
}

struct TabBarInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: TabBarOptions,
    scope: SignalScope,
    selected: Signal<usize>,
    labels: Vec<TabItem>,
    tabs: RefCell<Vec<Interactive>>,
    close_buttons: RefCell<Vec<Option<Interactive>>>,
    on_change: RefCell<Option<Rc<dyn Fn(usize, &mut App)>>>,
    on_close: RefCell<Option<Rc<dyn Fn(usize, &mut App)>>>,
    actions: TabActions,
    indicator_left: Transient<f32>,
    indicator_width: Transient<f32>,
    indicator_height: Transient<f32>,
    indicator_target: Cell<Option<(f32, f32, f32)>>,
    indicator_started: Cell<bool>,
    geometry: RefCell<Geometry>,
}

struct TabActions {
    left: Action,
    right: Action,
}

#[derive(Default)]
struct Geometry {
    window: Option<WindowId>,
    track: Option<NodeId>,
    row: Option<NodeId>,
    indicator: Option<NodeId>,
    tabs: Vec<NodeId>,
}

impl TabBar {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        labels: impl IntoIterator<Item = TabItem>,
        options: TabBarOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        Self {
            inner: Rc::new(TabBarInner {
                selected: app.signal_scoped(scope, 0usize),
                indicator_left: app.transient_scoped(scope, 0.0f32),
                indicator_width: app.transient_scoped(scope, 0.0f32),
                indicator_height: app.transient_scoped(scope, 0.0f32),
                indicator_target: Cell::new(None),
                indicator_started: Cell::new(false),
                actions: TabActions {
                    left: Action::Custom(format_smolstr!("{key}.left")),
                    right: Action::Custom(format_smolstr!("{key}.right")),
                },
                labels: labels.into_iter().collect(),
                tabs: RefCell::new(Vec::new()),
                close_buttons: RefCell::new(Vec::new()),
                on_change: RefCell::new(None),
                on_close: RefCell::new(None),
                geometry: RefCell::new(Geometry::default()),
                theme,
                options,
                scope,
                key,
            }),
        }
    }

    #[must_use]
    pub fn on_change(self, callback: impl Fn(usize, &mut App) + 'static) -> Self {
        *self.inner.on_change.borrow_mut() = Some(Rc::new(callback));
        self
    }

    #[must_use]
    pub fn on_close(self, callback: impl Fn(usize, &mut App) + 'static) -> Self {
        *self.inner.on_close.borrow_mut() = Some(Rc::new(callback));
        self
    }

    #[must_use]
    pub fn selected_index(&self, app: &App) -> usize {
        app.read_with(&self.inner.selected, |selected| *selected)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.labels.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.labels.is_empty()
    }

    #[must_use]
    pub fn tab_nodes(&self) -> Vec<NodeId> {
        self.inner.geometry.borrow().tabs.clone()
    }

    #[must_use]
    pub fn track_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().track
    }

    #[must_use]
    pub fn indicator_node(&self) -> Option<NodeId> {
        self.inner.geometry.borrow().indicator
    }

    #[must_use]
    pub fn indicator_rect(&self, app: &App) -> Option<LogicalRect> {
        let geometry = self.inner.geometry.borrow();
        let window = geometry.window?;
        let indicator = geometry.indicator?;
        app.window(window)?
            .tree()
            .get(indicator)
            .ok()?
            .layout_override()
            .map(|layout| layout.rect)
    }

    pub fn set_selected(&self, app: &mut App, index: usize) {
        let index = index.min(self.len().saturating_sub(1));
        app.write(&self.inner.selected, index);
    }

    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        if app.window(window).is_none() {
            return Err(EngineError::UnknownWindow { window });
        }
        self.inner.geometry.borrow_mut().window = Some(window);
        let inner = &self.inner;
        let count = inner.labels.len();
        let track_theme = inner.theme.clone();
        let track = NodeBuilder::boxed()
            .id(format_smolstr!("{}:track", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .width_full()
                    .padding(px(TRACK_PADDING))
                    .gap(px(TAB_GAP))
                    .build(),
            )
            .paint(PaintBuilder::new().rounded(px(TRACK_RADIUS)).build())
            .bind_style(move |cx| StylePatch {
                background: Some(track_theme.role(cx, Role::SurfaceSunken)),
                ..StylePatch::default()
            })
            .bind_a11y(move |_cx| A11yPatch {
                size_of_set: Some(count),
                ..A11yPatch::default()
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::List),
                ..A11yOverride::default()
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
        let row = NodeBuilder::boxed()
            .id(format_smolstr!("{}:row", inner.key))
            .style(
                StyleBuilder::new()
                    .flex_row()
                    .items_center()
                    .relative()
                    .width_full()
                    .gap(px(TAB_GAP))
                    .build(),
            )
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                track,
            )?;
        let indicator = if inner.options.indicator == IndicatorAnimation::Slide {
            let theme = inner.theme.clone();
            Some(
                NodeBuilder::boxed()
                    .id(format_smolstr!("{}:indicator", inner.key))
                    .layout_override(LogicalRect::default())
                    .paint(PaintBuilder::new().rounded(px(TRACK_RADIUS)).build())
                    .bind_style(move |cx| StylePatch {
                        background: Some(theme.role(cx, Role::SurfaceOverlay)),
                        ..StylePatch::default()
                    })
                    .build(
                        app.window_mut(window)
                            .ok_or(EngineError::UnknownWindow { window })?
                            .tree_mut(),
                        row,
                    )?,
            )
        } else {
            None
        };
        let closable = inner.options.closable && count > 1;
        let mut tabs = Vec::with_capacity(count);
        let mut close_buttons = Vec::with_capacity(count);
        let mut tab_nodes = Vec::with_capacity(count);
        for (index, item) in inner.labels.iter().enumerate() {
            let close = if closable {
                let close_key = format_smolstr!("{}:close:{index}", inner.key);
                let inner_close = inner.clone();
                Some(
                    Interactive::new(app, inner.theme.clone(), close_key)
                        .a11y_role(A11yRole::Button)
                        .label(format_smolstr!("Close {}", item.label))
                        .tab_stop(false)
                        .role_palette(close_palette())
                        .on_click(move |ctx| {
                            if let Some(callback) = inner_close.on_close.borrow().as_ref() {
                                callback(index, ctx.app_mut());
                            }
                        }),
                )
            } else {
                None
            };
            let tab_key = format_smolstr!("{}:tab:{index}", inner.key);
            let inner_select = inner.clone();
            let tab = Interactive::new(app, inner.theme.clone(), tab_key)
                .chrome(true)
                .with_unified_size(inner.options.size)
                .a11y_role(A11yRole::ListBoxOption)
                .a11y_set(index, count)
                .label(item.label.clone())
                .selected(index == 0)
                .role_palette(tab_palette(
                    inner.options.indicator == IndicatorAnimation::Slide,
                ))
                .on_click(move |ctx| {
                    select(&inner_select, ctx.app_mut(), index, true);
                });
            let label = item.label.clone();
            let close_for_content = close.clone();
            let close_watch = close.clone();
            let tab_for_label = tab.clone();
            let label_bind = tab.clone();
            let size = inner.options.size;
            let tab_node = tab.build(app, window, row, move |app, window, parent| {
                let text_style = TextStyle::new(size.text_size())
                    .with_wrap(TextWrap::NoWrap)
                    .with_overflow(TextOverflow::Ellipsis, 1);
                let text = NodeBuilder::text(label, text_style)
                    .style(StyleBuilder::new().min_width_0().build())
                    .bind_style(move |cx| StylePatch {
                        foreground: Some(label_bind.foreground_color(cx)),
                        ..StylePatch::default()
                    })
                    .build(
                        app.window_mut(window)
                            .ok_or(EngineError::UnknownWindow { window })?
                            .tree_mut(),
                        parent,
                    )?;
                tab_for_label.watch(text);
                if let Some(close) = close_for_content {
                    let close_icon = close.clone();
                    let close_bind = close.clone();
                    let side = close_side(size);
                    let close_node =
                        close.build(app, window, parent, move |app, window, parent| {
                            let icon = mount_dynamic_icon(
                                app,
                                window,
                                parent,
                                IconKind::X,
                                side,
                                move |cx| StylePatch {
                                    foreground: Some(close_bind.foreground_color(cx)),
                                    ..StylePatch::default()
                                },
                            )?;
                            close_icon.watch(icon);
                            Ok(())
                        })?;
                    set_square_close_style(app, window, close_node, side)?;
                }
                Ok(())
            })?;
            set_tab_max_width(app, window, tab_node)?;
            tabs.push(tab);
            close_buttons.push(close_watch);
            tab_nodes.push(tab_node);
        }
        *inner.tabs.borrow_mut() = tabs;
        *inner.close_buttons.borrow_mut() = close_buttons;
        {
            let mut geometry = inner.geometry.borrow_mut();
            geometry.track = Some(track);
            geometry.row = Some(row);
            geometry.indicator = indicator;
            geometry.tabs = tab_nodes;
        }
        if inner.options.indicator == IndicatorAnimation::Slide {
            self.sync_indicator_direct(app)?;
        }
        Ok(track)
    }

    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let Some(row) = self.inner.geometry.borrow().row else {
            return Ok(());
        };
        for (code, action, step) in [
            (KeyCode::ArrowLeft, self.inner.actions.left.clone(), -1isize),
            (
                KeyCode::ArrowRight,
                self.inner.actions.right.clone(),
                1isize,
            ),
        ] {
            app.bind_key_scoped(
                window,
                row,
                KeyBinding {
                    key: code,
                    modifiers: Modifiers::empty(),
                    action: action.clone(),
                },
            )?;
            let bar = self.clone();
            app.on_action(action, move |ctx| {
                bar.move_selection(ctx, step);
            });
        }
        Ok(())
    }

    pub fn mount(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        let node = self.build(app, window, parent)?;
        self.attach(app, window)?;
        Ok(node)
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let selected = self.selected_index(app);
        let mut changed = false;
        for (index, tab) in self.inner.tabs.borrow().iter().enumerate() {
            changed |= tab.sync(app)?;
            changed |= tab.set_selected(app, index == selected);
        }
        for close in self.inner.close_buttons.borrow().iter().flatten() {
            changed |= close.sync(app)?;
        }
        if self.inner.options.indicator == IndicatorAnimation::Slide {
            changed |= self.sync_indicator(app)?;
        }
        Ok(changed)
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        let mut changed = false;
        for tab in self.inner.tabs.borrow().iter() {
            changed |= tab.dispatch(app, event);
        }
        for close in self.inner.close_buttons.borrow().iter().flatten() {
            changed |= close.dispatch(app, event);
        }
        changed
    }

    pub fn release(&self, app: &mut App) -> usize {
        if let (Some(window), Some(row)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().row,
        ) {
            app.unbind_scope_keys(window, row);
        }
        app.off_action(&self.inner.actions.left);
        app.off_action(&self.inner.actions.right);
        let mut released = app.release_scope(self.inner.scope);
        for tab in self.inner.tabs.borrow().iter() {
            released += tab.release(app);
        }
        for close in self.inner.close_buttons.borrow().iter().flatten() {
            released += close.release(app);
        }
        self.inner.tabs.borrow_mut().clear();
        self.inner.close_buttons.borrow_mut().clear();
        released
    }

    fn move_selection(&self, ctx: &mut ActionCtx<'_>, step: isize) {
        let count = self.len();
        if count == 0 {
            return;
        }
        let current = self.selected_index(ctx.app_mut());
        let next = (current as isize + step).clamp(0, count as isize - 1) as usize;
        if next == current {
            return;
        }
        select(&self.inner, ctx.app_mut(), next, true);
        let window = ctx.window();
        if let Some(node) = self.inner.geometry.borrow().tabs.get(next).copied() {
            ctx.app_mut().set_focus(window, Some(node));
        }
    }

    fn sync_indicator_direct(&self, app: &mut App) -> Result<(), EngineError> {
        let Some((left, width, height)) = self.indicator_target(app) else {
            return Ok(());
        };
        app.write_transient(&self.inner.indicator_left, left);
        app.write_transient(&self.inner.indicator_width, width);
        app.write_transient(&self.inner.indicator_height, height);
        self.inner.indicator_target.set(Some((left, width, height)));
        self.inner.indicator_started.set(true);
        self.write_indicator_override(app)?;
        Ok(())
    }

    fn sync_indicator(&self, app: &mut App) -> Result<bool, EngineError> {
        let Some(target) = self.indicator_target(app) else {
            return Ok(false);
        };
        let mut changed = false;
        if self.inner.indicator_target.get() != Some(target) {
            self.inner.indicator_target.set(Some(target));
            let (left, width, height) = target;
            let nodes: Vec<NodeId> = self.inner.geometry.borrow().indicator.into_iter().collect();
            let spec = AnimationSpec {
                duration: self.inner.theme.tokens().animation_tick,
                easing: Easing::EaseOutCubic,
            };
            if self.inner.indicator_started.replace(true) {
                let Some(window) = self.inner.geometry.borrow().window else {
                    return Ok(changed);
                };
                for value in [
                    AnimatedValue::Scalar {
                        target: self.inner.indicator_left,
                        to: left,
                    },
                    AnimatedValue::Scalar {
                        target: self.inner.indicator_width,
                        to: width,
                    },
                    AnimatedValue::Scalar {
                        target: self.inner.indicator_height,
                        to: height,
                    },
                ] {
                    app.animate(window, value, &nodes, spec)?;
                }
            } else {
                app.write_transient(&self.inner.indicator_left, left);
                app.write_transient(&self.inner.indicator_width, width);
                app.write_transient(&self.inner.indicator_height, height);
            }
            changed = true;
        }
        changed |= self.write_indicator_override(app)?;
        Ok(changed)
    }

    fn write_indicator_override(&self, app: &mut App) -> Result<bool, EngineError> {
        let (Some(window), Some(indicator)) = (
            self.inner.geometry.borrow().window,
            self.inner.geometry.borrow().indicator,
        ) else {
            return Ok(false);
        };
        let rect = LogicalRect::new(
            px(*app.read_transient(&self.inner.indicator_left)),
            px(0.0),
            px(*app.read_transient(&self.inner.indicator_width)),
            px(*app.read_transient(&self.inner.indicator_height)),
        );
        let current = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree()
            .get(indicator)
            .ok()
            .and_then(|target| target.layout_override())
            .map(|layout| layout.rect);
        if current == Some(rect) {
            return Ok(false);
        }
        app.window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut()
            .set_layout_override(indicator, Some(LayoutOverride::new(rect)))?;
        Ok(true)
    }

    fn indicator_target(&self, app: &App) -> Option<(f32, f32, f32)> {
        let geometry = self.inner.geometry.borrow();
        let row = geometry.row?;
        let window = geometry.window?;
        let selected = self.selected_index(app);
        let tab = *geometry.tabs.get(selected)?;
        let tree = app.window(window)?.tree();
        let row_bounds = tree.get(row).ok()?.layout().bounds;
        let tab_bounds = tree.get(tab).ok()?.layout().bounds;
        Some((
            f32::from(tab_bounds.origin.x - row_bounds.origin.x),
            f32::from(tab_bounds.size.width),
            f32::from(tab_bounds.size.height),
        ))
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }
}

fn select(inner: &Rc<TabBarInner>, app: &mut App, index: usize, notify: bool) {
    let count = inner.labels.len();
    if count == 0 {
        return;
    }
    let index = index.min(count - 1);
    if !app.write(&inner.selected, index) {
        return;
    }
    if notify && let Some(callback) = inner.on_change.borrow().as_ref() {
        callback(index, app);
    }
}

fn tab_palette(slide: bool) -> RolePalette {
    RolePalette {
        normal: StateRoles {
            background: None,
            foreground: Role::FgMuted,
            border: None,
        },
        hover: StateRoles {
            background: (!slide).then_some(Role::VeilHover),
            foreground: Role::FgMuted,
            border: None,
        },
        active: StateRoles {
            background: (!slide).then_some(Role::SurfaceOverlay),
            foreground: Role::FgDefault,
            border: None,
        },
        disabled: StateRoles {
            background: None,
            foreground: Role::FgDisabled,
            border: None,
        },
    }
}

fn close_palette() -> RolePalette {
    RolePalette {
        normal: StateRoles {
            background: None,
            foreground: Role::FgMuted,
            border: None,
        },
        hover: StateRoles {
            background: Some(Role::VeilHover),
            foreground: Role::FgDefault,
            border: None,
        },
        active: StateRoles {
            background: Some(Role::VeilActive),
            foreground: Role::FgDefault,
            border: None,
        },
        disabled: StateRoles {
            background: None,
            foreground: Role::FgDisabled,
            border: None,
        },
    }
}

fn close_side(size: UnifiedSize) -> LogicalPx {
    match size {
        UnifiedSize::Small => px(12.0),
        UnifiedSize::Medium => px(16.0),
        UnifiedSize::Large => px(20.0),
    }
}

fn set_tab_max_width(app: &mut App, window: WindowId, node: NodeId) -> Result<(), EngineError> {
    let tree = app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    let mut style = tree.get(node)?.style().clone();
    style.max_width(LengthPercentageAuto::length(TAB_MAX_WIDTH));
    tree.set_style(node, style)
}

fn set_square_close_style(
    app: &mut App,
    window: WindowId,
    node: NodeId,
    side: LogicalPx,
) -> Result<(), EngineError> {
    let style = StyleBuilder::new()
        .flex_row()
        .items_center()
        .size(side, side)
        .build();
    app.window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut()
        .set_style(node, style)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, time::Duration};

    use ui_core::{
        a11y::A11yRole,
        event::{InputEvent, KeyCode, Modifiers, PointerButton},
        math::{LogicalPoint, LogicalRect, px},
        node::{id::NodeId, tree::NodeKind},
        paint::color::{Color, ColorResolver},
        test_support::app::TestApp,
        text::{TextOverflow, TextWrap},
    };

    use super::{IndicatorAnimation, TabBar, TabBarOptions, TabItem};
    use crate::{
        kit::size::UnifiedSize,
        theme::{appearance::Appearance, handle::ThemeHandle, resolved::Theme, role::Role},
    };

    struct Harness {
        app: TestApp,
        bar: TabBar,
        changed: Rc<Cell<Option<usize>>>,
        closed: Rc<Cell<Option<usize>>>,
    }

    fn harness(indicator: IndicatorAnimation, closable: bool, count: usize) -> Harness {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let changed = Rc::new(Cell::new(None));
        let closed = Rc::new(Cell::new(None));
        let change_sink = changed.clone();
        let close_sink = closed.clone();
        let labels = (0..count).map(|index| TabItem::new(format!("tab{index}")));
        let bar = TabBar::new(
            app.app_mut(),
            theme,
            "tabs",
            labels,
            TabBarOptions {
                size: UnifiedSize::Medium,
                indicator,
                closable,
            },
        )
        .on_change(move |index, _| change_sink.set(Some(index)))
        .on_close(move |index, _| close_sink.set(Some(index)));
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        bar.mount(app.app_mut(), window, root).expect("mount");
        let mut harness = Harness {
            app,
            bar,
            changed,
            closed,
        };
        settle(&mut harness);
        harness
    }

    fn settle(harness: &mut Harness) {
        for _ in 0..5 {
            harness.app.advance(Duration::from_millis(60));
            let _ = harness.app.drain().expect("frame");
            let _ = harness.bar.sync(harness.app.app_mut());
        }
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("frame");
        let _ = harness.bar.sync(harness.app.app_mut());
        let _ = harness.app.drain().expect("frame");
    }

    fn click(app: &mut TestApp, node: NodeId) {
        let bounds = app.probe().bounds(node).expect("bounds");
        let point = LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = app.inject(InputEvent::pointer_down(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = app.inject(InputEvent::pointer_up(
            point,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
    }

    fn tab_patch(harness: &Harness, index: usize) -> ui_core::paint::style::StylePatch {
        let window = harness.app.window_id();
        let node = harness.bar.tab_nodes()[index];
        harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("tab")
            .style_patch()
            .clone()
    }

    fn patch_at(harness: &Harness, node: NodeId) -> ui_core::paint::style::StylePatch {
        let window = harness.app.window_id();
        harness
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(node)
            .expect("node")
            .style_patch()
            .clone()
    }

    fn resolved(color: Color) -> [f32; 4] {
        ColorResolver.resolve(color).components
    }

    fn assert_color(actual: Option<Color>, expected: Color, message: &str) {
        let actual = actual.map(resolved);
        let expected = resolved(expected);
        let close = actual.is_some_and(|actual| {
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| (actual - expected).abs() < 1.0e-5)
        });
        assert!(close, "{message}（实际 {actual:?} / 期望 {expected:?}）");
    }

    #[test]
    fn clicking_a_tab_selects_it_and_reports_the_index() {
        let mut harness = harness(IndicatorAnimation::Fade, false, 3);
        assert_eq!(harness.bar.selected_index(harness.app.app()), 0);
        let node = harness.bar.tab_nodes()[1];
        click(&mut harness.app, node);
        let _ = harness.bar.sync(harness.app.app_mut());
        assert_eq!(harness.bar.selected_index(harness.app.app()), 1);
        assert_eq!(harness.changed.get(), Some(1));
    }

    #[test]
    fn arrow_keys_move_the_selection_within_the_scoped_tab_bar() {
        let mut harness = harness(IndicatorAnimation::Fade, false, 3);
        let window = harness.app.window_id();
        let first = harness.bar.tab_nodes()[0];
        assert!(harness.app.app_mut().set_focus(window, Some(first)));
        let _ = harness.app.inject(InputEvent::key_down(
            KeyCode::ArrowRight,
            Modifiers::empty(),
        ));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::ArrowRight, Modifiers::empty()));
        assert_eq!(harness.bar.selected_index(harness.app.app()), 1);
        assert_eq!(
            harness.app.app().focused(window),
            Some(harness.bar.tab_nodes()[1]),
            "焦点跟到新标签"
        );
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::ArrowLeft, Modifiers::empty()));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::ArrowLeft, Modifiers::empty()));
        assert_eq!(harness.bar.selected_index(harness.app.app()), 0);
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::ArrowLeft, Modifiers::empty()));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::ArrowLeft, Modifiers::empty()));
        assert_eq!(
            harness.bar.selected_index(harness.app.app()),
            0,
            "左边界不环绕"
        );
    }

    #[test]
    fn the_slide_indicator_moves_to_the_selected_tab() {
        let mut harness = harness(IndicatorAnimation::Slide, false, 3);
        let row = harness
            .bar
            .indicator_rect(harness.app.app())
            .expect("indicator");
        let first = harness.bar.tab_nodes()[0];
        let row_node = harness
            .app
            .app()
            .window(harness.app.window_id())
            .expect("window")
            .tree()
            .get(first)
            .expect("tab")
            .parent()
            .expect("row");
        let row_bounds = harness.app.probe().bounds(row_node).expect("row");
        let first_bounds = harness.app.probe().bounds(first).expect("tab");
        let expected_first = LogicalRect::new(
            first_bounds.origin.x - row_bounds.origin.x,
            px(0.0),
            first_bounds.size.width,
            first_bounds.size.height,
        );
        assert!(
            (row.origin.x - expected_first.origin.x).abs() < px(0.5)
                && (row.size.width - expected_first.size.width).abs() < px(0.5),
            "初始指示条应覆盖第一个标签（实际 {row:?} / 期望 {expected_first:?}）"
        );

        harness.bar.set_selected(harness.app.app_mut(), 2);
        settle(&mut harness);
        let rect = harness
            .bar
            .indicator_rect(harness.app.app())
            .expect("indicator");
        let last = harness.bar.tab_nodes()[2];
        let last_bounds = harness.app.probe().bounds(last).expect("tab");
        let expected_last = LogicalRect::new(
            last_bounds.origin.x - row_bounds.origin.x,
            px(0.0),
            last_bounds.size.width,
            last_bounds.size.height,
        );
        assert!(
            (rect.origin.x - expected_last.origin.x).abs() < px(0.5),
            "指示条必须移动到最后一个标签（实际 {} / 期望 {}）",
            rect.origin.x,
            expected_last.origin.x
        );
        assert!(
            (rect.size.width - expected_last.size.width).abs() < px(0.5),
            "指示条宽度跟随标签"
        );
    }

    #[test]
    fn fade_mode_paints_the_selected_tab_with_the_overlay_role() {
        let mut harness = harness(IndicatorAnimation::Fade, false, 3);
        assert!(harness.bar.indicator_node().is_none());
        let theme = Theme::of(Appearance::Light);
        assert_eq!(
            tab_patch(&harness, 0).background.map(resolved),
            Some(resolved(theme.role(Role::SurfaceOverlay))),
            "选中标签的背景"
        );
        let _ = harness.bar.sync(harness.app.app_mut());
        let node = harness.bar.tab_nodes()[2];
        click(&mut harness.app, node);
        settle(&mut harness);
        assert_eq!(
            tab_patch(&harness, 2).background.map(resolved),
            Some(resolved(theme.role(Role::SurfaceOverlay)))
        );
        assert_eq!(
            tab_patch(&harness, 0).background.map(resolved),
            Some(resolved(Color::TRANSPARENT)),
            "未选中标签透明"
        );
    }

    #[test]
    fn the_tab_bar_declares_list_and_list_item_semantics() {
        let mut app = TestApp::new();
        app.enable_a11y_capture();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let bar = TabBar::new(
            app.app_mut(),
            theme,
            "a11y-tabs",
            [
                TabItem::new("one"),
                TabItem::new("two"),
                TabItem::new("three"),
            ],
            TabBarOptions {
                size: UnifiedSize::Medium,
                indicator: IndicatorAnimation::Fade,
                closable: false,
            },
        );
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        bar.mount(app.app_mut(), window, root).expect("mount");
        let _ = app.drain().expect("a11y frame");
        let snapshot = app.a11y().expect("a11y");
        let track = snapshot
            .node_for(bar.track_node().expect("track"))
            .expect("track a11y");
        assert_eq!(track.role, A11yRole::List);
        assert_eq!(track.size_of_set, Some(3));
        let tabs = bar.tab_nodes();
        let second = snapshot.node_for(tabs[1]).expect("tab a11y");
        assert_eq!(second.role, A11yRole::ListBoxOption);
        assert_eq!(second.pos_in_set, Some(1));
        assert_eq!(second.size_of_set, Some(3));
        assert_eq!(second.selected, Some(false));
        assert_eq!(second.label_text(), Some("two"));
        let first = snapshot.node_for(tabs[0]).expect("tab a11y");
        assert_eq!(first.selected, Some(true));
    }

    #[test]
    fn the_close_button_reports_the_index_without_selecting() {
        let mut multi = harness(IndicatorAnimation::Fade, true, 3);
        let window = multi.app.window_id();
        let tab = multi.bar.tab_nodes()[1];
        let close = multi
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(tab)
            .expect("tab")
            .children()
            .last()
            .copied()
            .expect("close button");
        click(&mut multi.app, close);
        assert_eq!(multi.closed.get(), Some(1));
        assert_eq!(multi.bar.selected_index(multi.app.app()), 0);

        let single = harness(IndicatorAnimation::Fade, true, 1);
        let window = single.app.window_id();
        let children = single
            .app
            .app()
            .window(window)
            .expect("window")
            .tree()
            .get(single.bar.tab_nodes()[0])
            .expect("tab")
            .children()
            .len();
        assert_eq!(children, 1, "只剩一个标签时只画文字");
    }

    #[test]
    fn slide_mode_does_not_paint_the_selected_tab_background() {
        let harness = harness(IndicatorAnimation::Slide, false, 3);
        assert!(harness.bar.indicator_node().is_some());
        assert_eq!(
            tab_patch(&harness, 0).background.map(resolved),
            Some(resolved(Color::TRANSPARENT)),
            "Slide 的选中标签不得画 surface.overlay 填充"
        );
    }

    #[test]
    fn a_long_tab_label_is_capped_and_truncated() {
        let mut app = TestApp::new();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let long = "a_very_long_tab_label_that_must_not_stretch_the_bar";
        let bar = TabBar::new(
            app.app_mut(),
            theme,
            "long-tabs",
            [TabItem::new(long)],
            TabBarOptions::default(),
        );
        let window = app.window_id();
        let root = app.app().window(window).expect("window").tree().root();
        bar.mount(app.app_mut(), window, root).expect("mount");
        let _ = app.drain().expect("frame");
        let tab = bar.tab_nodes()[0];
        let bounds = app.probe().bounds(tab).expect("tab");
        assert!(
            bounds.size.width <= px(super::TAB_MAX_WIDTH + 0.5),
            "标签宽度上限 160 px（实际 {}）",
            bounds.size.width
        );
        let (text, text_style) = {
            let tree = app.app().window(window).expect("window").tree();
            let text = tree.get(tab).expect("tab").children()[0];
            let style = match tree.get(text).expect("text").kind() {
                NodeKind::Text(content) => content.style.clone(),
                other => panic!("expected Text, got {other:?}"),
            };
            (text, style)
        };
        assert_eq!(text_style.wrap, TextWrap::NoWrap);
        assert_eq!(text_style.overflow, TextOverflow::Ellipsis);
        assert_eq!(text_style.max_lines, 1);
        let text_bounds = app.probe().bounds(text).expect("text bounds");
        assert!(
            text_bounds.size.width < bounds.size.width,
            "文字必须被压到标签以内（{} vs {}）",
            text_bounds.size.width,
            bounds.size.width
        );
    }

    #[test]
    fn the_close_button_keeps_the_legacy_square_size_and_colors() {
        let mut harness = harness(IndicatorAnimation::Fade, true, 3);
        let tab = harness.bar.tab_nodes()[1];
        let window = harness.app.window_id();
        let (close, icon_size) = {
            let tree = harness.app.app().window(window).expect("window").tree();
            let close = *tree
                .get(tab)
                .expect("tab")
                .children()
                .last()
                .expect("close");
            let icon = tree.get(close).expect("close").children()[0];
            (
                close,
                tree.get(icon).expect("icon").style().inner().size.width,
            )
        };
        let bounds = harness.app.probe().bounds(close).expect("close bounds");
        assert_eq!(bounds.size.width, px(16.0), "Medium 关闭按钮 16 px");
        assert_eq!(bounds.size.height, px(16.0));
        assert_eq!(icon_size, taffy::style::Dimension::length(16.0), "X 满尺寸");
        let theme = Theme::of(Appearance::Light);
        assert_color(
            patch_at(&harness, close).foreground,
            theme.role(Role::FgMuted),
            "常态前景 fg.muted",
        );
        let point = LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let _ = harness
            .app
            .inject(InputEvent::pointer_move(point, Modifiers::empty()));
        let _ = harness.bar.sync(harness.app.app_mut());
        settle(&mut harness);
        assert_color(
            patch_at(&harness, close).background,
            theme.role(Role::VeilHover),
            "hover 背景 veil.hover",
        );
        assert_color(
            patch_at(&harness, close).foreground,
            theme.role(Role::FgDefault),
            "hover 前景 fg.default",
        );
    }

    #[test]
    fn releasing_the_tab_bar_unbinds_scoped_keys_and_action_handlers() {
        let mut harness = harness(IndicatorAnimation::Slide, true, 3);
        let window = harness.app.window_id();
        let row = harness.bar.inner.geometry.borrow().row.expect("row node");
        let left = harness.bar.inner.actions.left.clone();
        let right = harness.bar.inner.actions.right.clone();
        let tab_action = harness
            .bar
            .inner
            .tabs
            .borrow()
            .first()
            .expect("tab")
            .action_identity()
            .expect("tab click action");
        let close_action = harness
            .bar
            .inner
            .close_buttons
            .borrow()
            .first()
            .cloned()
            .flatten()
            .expect("close button")
            .action_identity()
            .expect("close click action");
        assert_eq!(
            harness.app.app().scoped_key_binding_count(window, row),
            2,
            "Direction 键绑在行节点上"
        );
        for action in [&left, &right, &tab_action, &close_action] {
            assert!(
                harness.app.app().has_action_handler(action),
                "{action:?} 必须在册"
            );
        }
        assert!(harness.bar.release(harness.app.app_mut()) > 0);
        assert_eq!(
            harness.app.app().scoped_key_binding_count(window, row),
            0,
            "release 必须摘掉 E8 scoped 键"
        );
        for action in [&left, &right, &tab_action, &close_action] {
            assert!(
                !harness.app.app().has_action_handler(action),
                "{action:?} 必须在 release 后摘除"
            );
        }
        assert_eq!(
            harness.bar.release(harness.app.app_mut()),
            0,
            "release 幂等"
        );
    }

    #[test]
    fn a_settled_tab_bar_produces_no_further_frames() {
        let mut harness = harness(IndicatorAnimation::Slide, false, 3);
        harness.bar.set_selected(harness.app.app_mut(), 2);
        settle(&mut harness);
        assert!(
            !harness.bar.sync(harness.app.app_mut()).expect("sync"),
            "稳态 sync 返回 false"
        );
        let record = harness.app.drain().expect("idle").clone();
        assert!(!record.rendered, "稳态不得出帧（I3）");
    }
}
