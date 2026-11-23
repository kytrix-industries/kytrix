use std::{cell::RefCell, fmt, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, KeyBinding, KeyCode, Modifiers},
    interaction::ActionCtx,
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    kit::{
        icon::IconKind,
        interactive::{Interactive, mount_dynamic_icon},
        layer::{Band, Layer},
        menu::{
            IconCache, ItemPanel, ItemPanelOptions, PanelContent, PanelMeasure, PanelRoles,
            PanelRow, PanelWidth, build_icon_slot,
        },
        overlay::{Anchor, AnimationRouter, OverlayError, Placement},
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{components::InteractiveVariant, handle::ThemeHandle, role::Role},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropdownItem {
    pub id: SmolStr,
    pub label: SmolStr,
    pub icon: Option<IconKind>,
    pub disabled: bool,
}

impl DropdownItem {
    #[must_use]
    pub fn new(id: impl Into<SmolStr>, label: impl Into<SmolStr>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            disabled: false,
        }
    }

    #[must_use]
    pub fn icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DropdownOptions {
    pub size: UnifiedSize,
    pub variant: InteractiveVariant,
    pub width: Option<f32>,
    pub max_height: f32,
    pub placement: Placement,
    pub gap: f32,
    pub label: Option<SmolStr>,
    pub placeholder: SmolStr,
}

impl Default for DropdownOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            variant: InteractiveVariant::Ghost,
            width: None,
            max_height: 320.0,
            placement: Placement::BOTTOM_START,
            gap: 4.0,
            label: None,
            placeholder: SmolStr::new_static("Select…"),
        }
    }
}

impl DropdownOptions {
    #[must_use]
    pub fn size(mut self, size: UnifiedSize) -> Self {
        self.size = size;
        self
    }

    #[must_use]
    pub fn variant(mut self, variant: InteractiveVariant) -> Self {
        self.variant = variant;
        self
    }

    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    #[must_use]
    pub fn max_height(mut self, max_height: f32) -> Self {
        self.max_height = max_height;
        self
    }

    #[must_use]
    pub fn placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }

    #[must_use]
    pub fn placeholder(mut self, placeholder: impl Into<SmolStr>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }
}

#[derive(Clone)]
pub struct Dropdown {
    inner: Rc<DropdownInner>,
}

#[derive(Default)]
struct DropdownState {
    window: Option<WindowId>,
    trigger: Option<NodeId>,
    trigger_width: f32,
    trigger_icon: Option<NodeId>,
    icon_shown: Option<IconKind>,
}

struct DropdownActions {
    trigger_click: Action,
    down: Action,
    up: Action,
}

struct DropdownInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: DropdownOptions,
    trigger: Interactive,
    panel: ItemPanel,
    icons: IconCache,
    items: RefCell<Rc<Vec<DropdownItem>>>,
    selected: Signal<Option<usize>>,
    display: Signal<SmolStr>,
    a11y_label: Signal<SmolStr>,
    on_select: RefCell<Option<Rc<dyn Fn(&mut App, SmolStr)>>>,
    actions: DropdownActions,
    state: RefCell<DropdownState>,
}

impl Dropdown {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
        options: DropdownOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        let trigger_click = Action::Custom(format_smolstr!("{key}.trigger"));
        let panel = ItemPanel::new(
            app,
            theme.clone(),
            layer,
            router,
            format_smolstr!("{key}:panel"),
            ItemPanelOptions {
                size: options.size,
                max_height: options.max_height,
                row_height: options.size.h(),
                band: Band::Popup,
                modal: true,
                placement: options.placement,
                gap: options.gap,
                roles: PanelRoles {
                    panel: Some(A11yRole::Menu),
                    row: A11yRole::MenuItem,
                },
                label: options.label.clone(),
                width: if options.width.is_some() {
                    PanelWidth::Exact
                } else {
                    PanelWidth::AtLeast
                },
                slot: crate::kit::layer::LayerSlot::Exclusive,
            },
        );
        let open_state = panel.open_state();
        let static_label = options
            .label
            .clone()
            .unwrap_or_else(|| options.placeholder.clone());
        let a11y_label = app.signal_scoped(scope, static_label.clone());
        let trigger = Interactive::new(app, theme.clone(), format_smolstr!("{key}:trigger"))
            .variant(options.variant)
            .with_unified_size(options.size)
            .a11y_role(A11yRole::Button)
            .label(static_label)
            .a11y_label_signal(a11y_label)
            .a11y_expanded(open_state)
            .action(trigger_click.clone());
        let inner = Rc::new(DropdownInner {
            actions: DropdownActions {
                trigger_click,
                down: Action::Custom(format_smolstr!("{key}.open-down")),
                up: Action::Custom(format_smolstr!("{key}.open-up")),
            },
            key,
            theme,
            options,
            trigger,
            panel,
            icons: IconCache::default(),
            items: RefCell::new(Rc::new(Vec::new())),
            selected: app.signal_scoped(scope, None),
            display: app.signal_scoped(scope, SmolStr::new_static("")),
            a11y_label,
            on_select: RefCell::new(None),
            state: RefCell::new(DropdownState::default()),
        });
        let dropdown = Self { inner };
        dropdown.install_handlers(app);
        dropdown
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    #[must_use]
    pub fn trigger_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().trigger
    }

    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.panel.overlay().panel_node()
    }

    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.panel.open_state()
    }

    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.inner.panel.is_open(app)
    }

    #[must_use]
    pub fn selected(&self, app: &App) -> Option<SmolStr> {
        let index = app.read_with(&self.inner.selected, |selected| *selected)?;
        self.inner
            .items
            .borrow()
            .get(index)
            .map(|item| item.id.clone())
    }

    #[must_use]
    pub fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.panel.highlighted(app)
    }

    #[must_use]
    pub fn items(&self) -> Rc<Vec<DropdownItem>> {
        self.inner.items.borrow().clone()
    }

    pub fn set_items(
        &self,
        app: &mut App,
        items: impl IntoIterator<Item = DropdownItem>,
    ) -> Result<(), EngineError> {
        let previous = self.selected(app);
        *self.inner.items.borrow_mut() = Rc::new(items.into_iter().collect());
        let next = previous.as_ref().and_then(|id| self.index_of(id));
        self.apply_selected(app, next);
        if let Some(window) = self.inner.state.borrow().window {
            self.resolve_icons(app, window)?;
        }
        self.inner.panel.set_content(app, self.panel_content())
    }

    pub fn set_selected(&self, app: &mut App, id: Option<&str>) {
        let next = id.and_then(|id| self.index_of(id));
        self.apply_selected(app, next);
    }

    pub fn on_select(self, callback: impl Fn(&mut App, SmolStr) + 'static) -> Self {
        *self.inner.on_select.borrow_mut() = Some(Rc::new(callback));
        self
    }

    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        self.inner.trigger.set_disabled(app, disabled)
    }

    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        self.inner.state.borrow_mut().window = Some(window);
        let size = self.inner.options.size;
        let display = self.inner.display;
        let selected = self.inner.selected;
        let theme = self.inner.theme.clone();
        let placeholder = self.inner.options.placeholder.clone();
        let trigger = self.inner.trigger.clone();
        let trigger_text = trigger.clone();
        let trigger_text_move = trigger_text.clone();
        let trigger_caret = trigger.clone();
        let current_icon = self.selected_icon(app);
        self.inner.icons.resolve(
            app,
            window,
            current_icon.unwrap_or(IconKind::X),
            size.icon_size(),
        )?;
        let icon_resource = self
            .inner
            .icons
            .snapshot()
            .get(current_icon.unwrap_or(IconKind::X), size.icon_size())
            .ok_or(EngineError::UnknownWindow { window })?;
        let icon_slot = Rc::new(std::cell::Cell::new(None));
        let icon_slot_inner = icon_slot.clone();
        let theme_icon = theme.clone();
        let node = trigger.build(app, window, parent, move |app, window, parent| {
            let side = if current_icon.is_some() {
                size.icon_size()
            } else {
                0.0
            };
            let icon = NodeBuilder::image(icon_resource, ui_core::node::image::ImageFit::Fill)
                .style(StyleBuilder::new().size(side, side).build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_icon.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .a11y(ui_core::a11y::A11yOverride {
                    hidden: true,
                    ..ui_core::a11y::A11yOverride::default()
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    parent,
                )?;
            icon_slot_inner.set(Some(icon));
            let theme_text = theme.clone();
            let text = NodeBuilder::text(
                SmolStr::new_static(""),
                TextStyle::new(size.text_size())
                    .with_wrap(TextWrap::NoWrap)
                    .with_overflow(TextOverflow::Ellipsis, 1),
            )
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_text(move |cx| {
                let text = cx.read(&display).clone();
                if text.is_empty() {
                    placeholder.clone()
                } else {
                    text
                }
            })
            .bind_style(move |cx| {
                let foreground = if cx.read(&selected).is_none() {
                    theme_text.role(cx, Role::FgMuted)
                } else {
                    trigger_text_move.foreground_color(cx)
                };
                StylePatch {
                    foreground: Some(foreground),
                    ..StylePatch::default()
                }
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
            trigger_text.watch(text);
            let theme_caret = theme.clone();
            let caret = mount_dynamic_icon(
                app,
                window,
                parent,
                IconKind::CaretDown,
                size.icon_size(),
                move |cx| StylePatch {
                    foreground: Some(theme_caret.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                },
            )?;
            trigger_caret.watch(caret);
            Ok(())
        })?;
        {
            let mut state = self.inner.state.borrow_mut();
            state.trigger = Some(node);
            state.trigger_icon = icon_slot.get();
            state.icon_shown = current_icon;
        }
        Ok(node)
    }

    pub fn attach(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let Some(trigger) = self.inner.state.borrow().trigger else {
            return Ok(());
        };
        for (key, action) in [
            (KeyCode::ArrowDown, self.inner.actions.down.clone()),
            (KeyCode::ArrowUp, self.inner.actions.up.clone()),
        ] {
            app.bind_key_scoped(
                window,
                trigger,
                KeyBinding {
                    key,
                    modifiers: Modifiers::empty(),
                    action,
                },
            )?;
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

    pub fn open(&self, app: &mut App) -> Result<bool, OverlayError> {
        let (Some(window), Some(trigger)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().trigger,
        ) else {
            return Ok(false);
        };
        self.measure_trigger(app);
        self.resolve_icons(app, window)
            .map_err(OverlayError::from)?;
        let width = self
            .inner
            .options
            .width
            .unwrap_or_else(|| self.inner.state.borrow().trigger_width.max(1.0));
        self.inner
            .panel
            .set_content(app, self.panel_content())
            .map_err(OverlayError::from)?;
        let opened = self
            .inner
            .panel
            .open(app, window, Anchor::node(trigger), width)?;
        if opened {
            let row = app
                .read_with(&self.inner.selected, |selected| *selected)
                .or_else(|| self.first_enabled());
            self.inner.panel.set_highlighted(app, row);
            let _ = self.inner.panel.focus(app);
        }
        Ok(opened)
    }

    pub fn close(&self, app: &mut App) -> bool {
        self.inner.panel.close(app)
    }

    pub fn close_now(&self, app: &mut App) -> bool {
        self.inner.panel.close_now(app)
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.trigger.sync(app)?;
        changed |= self.sync_trigger_a11y(app);
        changed |= self.sync_trigger_icon(app)?;
        if self.is_open(app) {
            self.measure_trigger(app);
            if self.inner.options.width.is_none() {
                let width = self.inner.state.borrow().trigger_width.max(1.0);
                self.inner.panel.resize(app, width)?;
            }
        }
        changed |= self.inner.panel.sync(app, true)?;
        Ok(changed)
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.inner.trigger.dispatch(app, event)
    }

    pub fn release(&self, app: &mut App) -> usize {
        for action in [
            &self.inner.actions.trigger_click,
            &self.inner.actions.down,
            &self.inner.actions.up,
        ] {
            app.off_action(action);
        }
        self.inner.trigger.release(app) + self.inner.panel.release(app)
    }

    fn panel_content(&self) -> PanelContent {
        let items = self.inner.items.borrow().clone();
        let rows: Vec<PanelRow> = items
            .iter()
            .map(|item| PanelRow {
                height: self.inner.options.size.h(),
                disabled: item.disabled,
            })
            .collect();
        let theme = self.inner.theme.clone();
        let size = self.inner.options.size;
        let labels: Vec<SmolStr> = items.iter().map(|item| item.label.clone()).collect();
        let icons = Rc::new(self.inner.icons.snapshot());
        let has_icon = items.iter().any(|item| item.icon.is_some());
        let build: Rc<dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>> = {
            let items = items.clone();
            Rc::new(move |tree, parent, index| {
                let Some(item) = items.get(index) else {
                    return Ok(());
                };
                build_icon_slot(tree, parent, &theme, &icons, item.icon, size, has_icon)?;
                let theme = theme.clone();
                NodeBuilder::text(
                    item.label.clone(),
                    TextStyle::new(size.text_size())
                        .with_wrap(TextWrap::NoWrap)
                        .with_overflow(TextOverflow::Ellipsis, 1),
                )
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme.role(cx, Role::FgDefault)),
                    ..StylePatch::default()
                })
                .build(tree, parent)
                .map(|_| ())
            })
        };
        let disabled: Vec<bool> = items.iter().map(|item| item.disabled).collect();
        let measure_labels = labels.clone();
        let a11y: Rc<dyn Fn(usize) -> A11yPatch> = Rc::new(move |index| match labels.get(index) {
            Some(label) => A11yPatch {
                role: Some(A11yRole::MenuItem),
                label: Some(label.clone()),
                disabled: disabled.get(index).copied(),
                ..A11yPatch::default()
            },
            None => A11yPatch {
                hidden: Some(true),
                ..A11yPatch::default()
            },
        });
        let measure = Some(PanelMeasure {
            labels: measure_labels,
            extra: if has_icon { size.icon_size() } else { 0.0 },
        });
        PanelContent {
            rows,
            build,
            a11y,
            measure,
        }
    }

    fn install_handlers(&self, app: &mut App) {
        let this = self.clone();
        let disabled = self.inner.trigger.disabled_signal();
        let click = self.inner.actions.trigger_click.clone();
        app.on_action(click, move |ctx: &mut ActionCtx<'_>| {
            if *ctx.read(&disabled) {
                return;
            }
            let app = ctx.app_mut();
            if this.is_open(app) {
                this.close(app);
            } else {
                let _ = this.open(app);
            }
        });
        let this = self.clone();
        app.on_action(self.inner.actions.down.clone(), move |ctx| {
            let app = ctx.app_mut();
            let _ = this.open(app);
            let row = app
                .read_with(&this.inner.selected, |selected| *selected)
                .or_else(|| this.first_enabled());
            this.inner.panel.set_highlighted(app, row);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.up.clone(), move |ctx| {
            let app = ctx.app_mut();
            let _ = this.open(app);
            let items = this.inner.items.borrow();
            let last = (0..items.len()).rev().find(|index| !items[*index].disabled);
            drop(items);
            this.inner.panel.set_highlighted(app, last);
        });
        let this = self.clone();
        self.inner.panel.on_activate(move |app, row| {
            this.select_row(app, row);
        });
        let this = self.clone();
        self.inner.panel.on_close(move |app| {
            let (Some(window), Some(trigger)) = (
                this.inner.state.borrow().window,
                this.inner.state.borrow().trigger,
            ) else {
                return;
            };
            let _ = app.set_focus(window, Some(trigger));
        });
    }

    fn select_row(&self, app: &mut App, row: usize) {
        let item = self.inner.items.borrow().get(row).cloned();
        let Some(item) = item else {
            return;
        };
        if item.disabled {
            return;
        }
        self.apply_selected(app, Some(row));
        let _ = self.close(app);
        if let Some(callback) = self.inner.on_select.borrow().clone() {
            callback(app, item.id);
        }
    }

    fn apply_selected(&self, app: &mut App, index: Option<usize>) {
        let label = index
            .and_then(|index| {
                self.inner
                    .items
                    .borrow()
                    .get(index)
                    .map(|item| item.label.clone())
            })
            .unwrap_or_default();
        app.write(&self.inner.selected, index);
        app.write(&self.inner.display, label);
        self.sync_trigger_a11y(app);
        if let Some(index) = index {
            self.inner.panel.set_highlighted(app, Some(index));
        }
    }

    fn sync_trigger_a11y(&self, app: &mut App) -> bool {
        if self.inner.options.label.is_some() {
            return false;
        }
        let next = app
            .read_with(&self.inner.selected, |selected| *selected)
            .and_then(|index| {
                self.inner
                    .items
                    .borrow()
                    .get(index)
                    .map(|item| item.label.clone())
            })
            .unwrap_or_else(|| self.inner.options.placeholder.clone());
        app.write(&self.inner.a11y_label, next)
    }

    fn selected_icon(&self, app: &App) -> Option<IconKind> {
        let index = app.read_with(&self.inner.selected, |selected| *selected)?;
        self.inner
            .items
            .borrow()
            .get(index)
            .and_then(|item| item.icon)
    }

    fn resolve_icons(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let size = self.inner.options.size.icon_size();
        let items = self.inner.items.borrow().clone();
        let mut kinds: Vec<IconKind> = items.iter().filter_map(|item| item.icon).collect();
        kinds.extend(self.selected_icon(app));
        kinds.sort_unstable();
        kinds.dedup();
        for kind in kinds {
            self.inner.icons.resolve(app, window, kind, size)?;
        }
        Ok(())
    }

    fn sync_trigger_icon(&self, app: &mut App) -> Result<bool, EngineError> {
        let (Some(window), Some(node)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().trigger_icon,
        ) else {
            return Ok(false);
        };
        let desired = self.selected_icon(app);
        {
            let state = self.inner.state.borrow();
            if state.icon_shown == desired {
                return Ok(false);
            }
        }
        let size = self.inner.options.size.icon_size();
        let mut changed = false;
        if let Some(kind) = desired {
            self.inner.icons.resolve(app, window, kind, size)?;
            changed = true;
        }
        let resource = desired.and_then(|kind| self.inner.icons.snapshot().get(kind, size));
        if let Some(target) = app.window_mut(window) {
            if let Ok(current) = target.tree().get(node) {
                let mut style = current.style().clone();
                let side = if desired.is_some() { size } else { 0.0 };
                style.size_length(side, side);
                let _ = target.tree_mut().set_style(node, style);
            }
            if let Some(resource) = resource {
                let _ = target.set_image_resource(node, resource);
            }
        }
        self.inner.state.borrow_mut().icon_shown = desired;
        Ok(changed)
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.inner
            .items
            .borrow()
            .iter()
            .position(|item| item.id == id)
    }

    fn first_enabled(&self) -> Option<usize> {
        self.inner
            .items
            .borrow()
            .iter()
            .position(|item| !item.disabled)
    }

    fn measure_trigger(&self, app: &App) {
        let (Some(window), Some(trigger)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().trigger,
        ) else {
            return;
        };
        let Some(width) = app
            .window(window)
            .and_then(|target| target.tree().get(trigger).ok())
            .map(|node| node.layout().bounds.size.width)
        else {
            return;
        };
        self.inner.state.borrow_mut().trigger_width = width;
    }
}

impl fmt::Debug for Dropdown {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Dropdown")
            .field("key", &self.inner.key)
            .finish()
    }
}

#[cfg(test)]
mod tests {

    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        time::Duration,
    };

    use smol_str::SmolStr;
    use ui_core::{
        a11y::A11yRole,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{Point, Rect},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
        paint::style_builder::StyleBuilder,
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{Dropdown, DropdownItem, DropdownOptions};
    use crate::{
        kit::{
            icon::IconKind,
            layer::Layer,
            overlay::AnimationRouter,
            test_support::{a11y_snapshot_without_oracle, overlay_wrapper},
        },
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        selected: Rc<RefCell<Vec<SmolStr>>>,
        background_clicks: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let clicks = Rc::new(Cell::new(0));
            let sink = clicks.clone();
            NodeBuilder::boxed()
                .id("background")
                .layout_override(Rect::new(600.0, 500.0, 80.0, 40.0))
                .on_click(Action::Custom(SmolStr::new_static("background.click")))
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("背景节点");
            app.app_mut().on_action(
                Action::Custom(SmolStr::new_static("background.click")),
                move |_| sink.set(sink.get() + 1),
            );
            Self {
                app,
                window,
                theme,
                layer: Layer::new(),
                router: AnimationRouter::new(),
                selected: Rc::new(RefCell::new(Vec::new())),
                background_clicks: clicks,
            }
        }

        fn dropdown(&mut self, options: DropdownOptions) -> Dropdown {
            let selected = self.selected.clone();
            Dropdown::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                "dropdown",
                options,
            )
            .on_select(move |_, id| selected.borrow_mut().push(id))
        }

        fn mount(&mut self, dropdown: &Dropdown, items: impl IntoIterator<Item = DropdownItem>) {
            let root = self
                .app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .root();
            dropdown.set_items(self.app.app_mut(), items).expect("条目");
            dropdown
                .mount(self.app.app_mut(), self.window, root)
                .expect("挂载");
            self.settle(dropdown);
        }

        fn settle(&mut self, dropdown: &Dropdown) {
            for _ in 0..6 {
                self.app.advance(Duration::from_millis(40));
                let _ = self.app.drain().expect("帧");
                let _ = dropdown.sync(self.app.app_mut());
            }
            self.app.advance(Duration::from_millis(200));
            let _ = self.app.drain().expect("帧");
            let _ = dropdown.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
        }

        fn focus_trigger(&mut self, dropdown: &Dropdown) {
            let trigger = dropdown.trigger_node().expect("触发器");
            self.app.app_mut().set_focus(self.window, Some(trigger));
        }

        fn key(&mut self, dropdown: &Dropdown, code: KeyCode) {
            let _ = self
                .app
                .inject(InputEvent::key_down(code, Modifiers::empty()));
            let _ = self
                .app
                .inject(InputEvent::key_up(code, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
            let _ = dropdown.sync(self.app.app_mut());
        }

        fn click(&mut self, dropdown: &Dropdown, at: Point) {
            let _ = self.app.inject(InputEvent::pointer_down(
                at,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                at,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            self.settle(dropdown);
        }

        fn bounds(&self, node: NodeId) -> Rect {
            self.app.probe().bounds(node).expect("布局盒")
        }

        fn center(&self, node: NodeId) -> Point {
            let bounds = self.bounds(node);
            Point::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        fn find(&self, debug_id: &str) -> Option<NodeId> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![tree.root()];
            while let Some(node) = stack.pop() {
                if tree.debug_id(node) == Some(debug_id) {
                    return Some(node);
                }
                if let Ok(target) = tree.get(node) {
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            None
        }

        fn row(&self, index: usize) -> NodeId {
            self.find(&format!("dropdown:panel:list:row:{index}"))
                .unwrap_or_else(|| panic!("第 {index} 行未挂载"))
        }

        fn row_center(&self, index: usize) -> Point {
            let bounds = self.bounds(self.row(index));
            Point::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        fn trigger_text(&self, dropdown: &Dropdown) -> SmolStr {
            let trigger = dropdown.trigger_node().expect("触发器");
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![trigger];
            while let Some(node) = stack.pop() {
                if let Ok(target) = tree.get(node) {
                    if let NodeKind::Text(content) = target.kind() {
                        return content.text.clone();
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            panic!("触发器里没有文本节点")
        }

        fn mounted_rows(&self) -> usize {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![tree.root()];
            let mut rows = 0;
            while let Some(node) = stack.pop() {
                if let Some(id) = tree.debug_id(node)
                    && id.starts_with("dropdown:panel:list:row:")
                    && !id.ends_with(":content")
                    && !id.contains(":hit:")
                {
                    rows += 1;
                }
                if let Ok(target) = tree.get(node) {
                    stack.extend(target.children().iter().copied());
                }
            }
            rows
        }

        fn idles(&mut self) -> bool {
            !self.app.drain().expect("帧").rendered
        }

        fn visible_images(&self, node: NodeId) -> usize {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![node];
            let mut count = 0;
            while let Some(current) = stack.pop() {
                if let Ok(target) = tree.get(current) {
                    if matches!(target.kind(), NodeKind::Image(_))
                        && target
                            .style()
                            .inner()
                            .size
                            .width
                            .into_option()
                            .is_some_and(|width| width > 0.0)
                    {
                        count += 1;
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            count
        }
    }

    fn items() -> Vec<DropdownItem> {
        vec![
            DropdownItem::new("a", "Alpha"),
            DropdownItem::new("b", "Beta"),
            DropdownItem::new("c", "Gamma").disabled(true),
            DropdownItem::new("d", "Delta"),
        ]
    }

    #[test]
    fn pressing_the_trigger_opens_the_panel_and_pressing_it_again_closes_it() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        let center = harness.center(trigger);

        harness.click(&dropdown, center);
        assert!(dropdown.is_open(harness.app.app()), "点击触发器打开面板");
        assert!(dropdown.panel_node().is_some());

        harness.click(&dropdown, center);
        assert!(
            !dropdown.is_open(harness.app.app()),
            "再点触发器关闭面板（触发器在浮层外）"
        );
    }

    #[test]
    fn choosing_an_item_updates_the_selection_the_trigger_and_the_callback() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        let at = harness.row_center(1);
        harness.click(&dropdown, at);
        assert_eq!(
            harness.selected.borrow().as_slice(),
            &[SmolStr::new_static("b")],
            "选中项回调"
        );
        assert_eq!(
            dropdown.selected(harness.app.app()),
            Some(SmolStr::new_static("b"))
        );
        assert!(!dropdown.is_open(harness.app.app()), "选中即关面板");
        assert_eq!(harness.trigger_text(&dropdown), "Beta", "触发器显示选中项");
    }

    #[test]
    fn arrow_down_on_the_trigger_opens_with_the_selected_item_highlighted() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        dropdown.set_selected(harness.app.app_mut(), Some("d"));
        harness.settle(&dropdown);
        harness.focus_trigger(&dropdown);

        harness.key(&dropdown, KeyCode::ArrowDown);
        assert!(dropdown.is_open(harness.app.app()), "↓ 打开面板");
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(3),
            "高亮落选中项"
        );
    }

    #[test]
    fn end_moves_the_highlight_to_the_last_item_and_enter_chooses_it() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        harness.key(&dropdown, KeyCode::End);
        assert_eq!(dropdown.highlighted(harness.app.app()), Some(3));
        harness.key(&dropdown, KeyCode::Home);
        assert_eq!(dropdown.highlighted(harness.app.app()), Some(0));
        harness.key(&dropdown, KeyCode::End);
        harness.key(&dropdown, KeyCode::Enter);
        assert_eq!(
            harness.selected.borrow().as_slice(),
            &[SmolStr::new_static("d")]
        );
    }

    #[test]
    fn a_disabled_item_cannot_be_chosen_and_keeps_the_panel_open() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        let at = harness.row_center(2);
        harness.click(&dropdown, at);
        assert!(harness.selected.borrow().is_empty(), "禁用条目不可选");
        assert!(dropdown.is_open(harness.app.app()), "面板保持打开");
    }

    #[test]
    fn the_panel_is_as_wide_as_its_trigger() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        let trigger_width = harness.bounds(trigger).size.width;
        let panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert!(
            (panel.size.width - trigger_width).abs() < 0.5,
            "面板宽度对齐触发器：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn a_long_item_list_is_virtualized_and_scrolls_to_the_end() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions {
            max_height: 200.0,
            ..DropdownOptions::default()
        });
        let many: Vec<DropdownItem> = (0..200)
            .map(|index| {
                let id = format!("item-{index}");
                DropdownItem::new(id.clone(), id)
            })
            .collect();
        harness.mount(&dropdown, many);
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        let panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert!(
            panel.size.height <= 200.0 + 0.5,
            "面板高度被 max_height 截住"
        );
        assert!(
            harness.mounted_rows() < 25,
            "长列表只挂可见窗口内的行（实际 {}）",
            harness.mounted_rows()
        );
        harness.key(&dropdown, KeyCode::End);
        assert_eq!(dropdown.highlighted(harness.app.app()), Some(199));
        assert!(harness.find("dropdown:panel:list:row:199").is_some());
    }

    #[test]
    fn a_press_outside_closes_the_panel_and_returns_focus_to_the_trigger() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()));

        harness.click(&dropdown, Point::new(640.0, 520.0));
        assert!(!dropdown.is_open(harness.app.app()), "点击外部关闭");
        assert_eq!(
            harness.background_clicks.get(),
            0,
            "外部按下被模态浮层消费（已接受偏差 F5：不穿透背景）"
        );
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger),
            "关闭后焦点回到触发器"
        );
    }

    #[test]
    fn escape_closes_the_panel() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        harness.key(&dropdown, KeyCode::Escape);
        assert!(!dropdown.is_open(harness.app.app()), "Esc 关闭面板");
    }

    #[test]
    fn a_closed_dropdown_does_not_swallow_a_click_behind_it() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        harness.click(&dropdown, Point::new(640.0, 520.0));
        assert_eq!(harness.background_clicks.get(), 1, "关闭态不遮背景点击");
    }

    #[test]
    fn the_panel_declares_menu_and_menu_item_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let panel = dropdown.panel_node().expect("面板");
        let wrapper = overlay_wrapper(harness.app.app(), harness.window, panel);
        assert_eq!(
            snapshot.node_for(wrapper).expect("包装").role,
            A11yRole::Menu
        );
        assert_eq!(
            snapshot.node_for(panel).expect("面板").role,
            A11yRole::GenericContainer
        );
        let labels: Vec<String> = snapshot
            .nodes()
            .iter()
            .filter(|node| node.role == A11yRole::MenuItem)
            .filter_map(|node| node.label.as_ref().map(|label| label.text().to_owned()))
            .collect();
        assert!(
            labels.contains(&"Alpha".to_owned()),
            "行可访问名：{labels:?}"
        );
        assert!(labels.contains(&"Gamma".to_owned()), "禁用行也在 a11y 树里");
    }

    #[test]
    fn a_settled_dropdown_with_no_input_requests_no_frames() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        assert!(!dropdown.sync(harness.app.app_mut()).expect("同步"));
        assert!(harness.idles(), "稳态不出帧（I3）");
    }

    #[test]
    fn set_items_keeps_no_selection_and_shows_the_placeholder() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default().placeholder("请选择"));
        harness.mount(&dropdown, items());
        assert_eq!(
            dropdown.selected(harness.app.app()),
            None,
            "set_items 不自动选中首项"
        );
        assert_eq!(harness.trigger_text(&dropdown), "请选择", "占位文本可见");
    }

    #[test]
    fn a_long_item_grows_the_panel_past_its_trigger() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(
            &dropdown,
            vec![
                DropdownItem::new(
                    "long",
                    "A very long dropdown entry that cannot fit the trigger width at all",
                ),
                DropdownItem::new("short", "B"),
            ],
        );
        let trigger = dropdown.trigger_node().expect("触发器");
        let trigger_width = harness.bounds(trigger).size.width;
        harness.click(&dropdown, harness.center(trigger));
        let panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert!(
            panel.size.width > trigger_width + 100.0,
            "长条目把面板撑宽（旧 min_w 下限口径）：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn an_explicit_width_stays_exact_for_long_items() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default().width(120.0));
        harness.mount(
            &dropdown,
            vec![
                DropdownItem::new(
                    "long",
                    "A very long dropdown entry that cannot fit the stated panel width at all",
                ),
                DropdownItem::new("short", "B"),
            ],
        );
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        let panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert!(
            (panel.size.width - 120.0).abs() < 0.5,
            "显式宽度精确生效、不随长条目增长：{}",
            panel.size.width
        );
        let row = harness.bounds(harness.row(0));
        assert!(
            row.size.width <= 120.0 - 8.0 + 0.5,
            "长条目被面板宽度约束（截断 / 换行）：行宽 {}",
            row.size.width
        );
    }

    #[test]
    fn item_icons_are_drawn_in_the_rows_and_on_the_trigger() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(
            &dropdown,
            vec![
                DropdownItem::new("a", "Alpha").icon(IconKind::Circle),
                DropdownItem::new("b", "Beta"),
            ],
        );
        let trigger = dropdown.trigger_node().expect("触发器");
        assert_eq!(
            harness.visible_images(trigger),
            1,
            "没有选中项时触发器只有 caret"
        );

        harness.click(&dropdown, harness.center(trigger));
        let panel = dropdown.panel_node().expect("面板");
        assert_eq!(
            harness.visible_images(panel),
            1,
            "只有带图标的条目画图标（其余行留占位槽）"
        );

        harness.click(&dropdown, harness.row_center(0));
        harness.settle(&dropdown);
        assert_eq!(
            harness.visible_images(trigger),
            2,
            "选中带图标的条目后触发器画该图标 + caret"
        );
    }

    #[test]
    fn the_trigger_reports_the_selection_and_the_expanded_state() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dropdown = harness.dropdown(DropdownOptions::default().placeholder("请选择"));
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");

        harness.settle(&dropdown);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("请选择"), "占位文本是可访问名");
        assert_eq!(record.expanded, Some(false), "关闭态 aria-expanded=false");

        harness.click(&dropdown, harness.center(trigger));
        harness.settle(&dropdown);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.expanded, Some(true), "打开态 aria-expanded=true");

        harness.click(&dropdown, harness.row_center(1));
        harness.settle(&dropdown);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.label_text(), Some("Beta"), "可访问名跟随选中项");
        assert_eq!(record.expanded, Some(false), "关闭后 expanded 复原");
    }

    #[test]
    fn an_open_dropdown_consumes_background_scroll() {
        let mut harness = Harness::new();
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let page = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(800.0)
                        .height(600.0)
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("页面")
        };
        let handle = harness
            .app
            .app_mut()
            .make_scroll(
                harness.window,
                page,
                ui_core::interaction::ScrollConfig::default(),
            )
            .expect("滚动容器");
        harness
            .app
            .app_mut()
            .set_scroll_content(&handle, ui_core::math::Size::new(800.0, 2400.0))
            .expect("内容");

        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()));

        let wheel = |harness: &mut Harness, dropdown: &Dropdown| {
            let _ = harness.app.inject(InputEvent::scroll_lines(
                Point::new(700.0, 500.0),
                Point::new(0.0, -2.0),
                Modifiers::empty(),
            ));
            harness.settle(dropdown);
            for _ in 0..8 {
                harness.app.advance(Duration::from_millis(40));
                let _ = harness.app.drain().expect("帧");
            }
        };
        wheel(&mut harness, &dropdown);
        assert_eq!(
            harness
                .app
                .app()
                .scroll_state(&handle)
                .expect("滚动状态")
                .offset
                .y,
            0.0,
            "面板打开时背景滚轮被消费"
        );

        harness.click(&dropdown, harness.center(trigger));
        wheel(&mut harness, &dropdown);
        assert!(
            harness
                .app
                .app()
                .scroll_state(&handle)
                .expect("滚动状态")
                .offset
                .y
                > 50.0,
            "无浮层时背景可滚"
        );
    }
}
