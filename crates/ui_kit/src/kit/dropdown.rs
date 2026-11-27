use std::{cell::RefCell, fmt, rc::Rc};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, KeyBinding, KeyCode, Modifiers},
    interaction::ActionCtx,
    math::{LogicalPx, px},
    node::{builder::NodeBuilder, id::NodeId, signal::Signal, tree::NodeTree},
    paint::{style::StylePatch, style_builder::StyleBuilder},
    popup::{
        Alignment, Anchor, DismissPolicy, DismissReason, EscapePolicy, FocusPolicy, HasPopup,
        OutsidePress, PlacementSpec, PopupError, PopupTier, RestoreFocus, Side,
    },
    text::{TextOverflow, TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    kit::{
        icon::IconKind,
        interactive::{Interactive, mount_dynamic_icon},
        menu::{
            IconSnapshot, ItemPanel, ItemPanelOptions, PanelContent, PanelMeasure, PanelOpen,
            PanelRoles, PanelRow, PanelWidth, build_icon_slot, popup_error,
        },
        popup_panel::{AnchorChange, AnchorWatch},
        size::{UnifiedSizable, UnifiedSize},
    },
    theme::{components::InteractiveVariant, handle::ThemeHandle, role::Role},
};

const DROPDOWN_DISMISS: DismissPolicy = DismissPolicy::Chain {
    outside: OutsidePress::Replay,
    escape: EscapePolicy::CloseTopmost,
    anchor_lost: true,
    focus_lost: true,
};

const DROPDOWN_ROLES: PanelRoles = PanelRoles {
    panel: Some(A11yRole::Menu),
    row: A11yRole::MenuItem,
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
    pub width: Option<LogicalPx>,
    pub max_height: LogicalPx,
    pub placement: PlacementSpec,
    pub label: Option<SmolStr>,
    pub placeholder: SmolStr,
}

impl Default for DropdownOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            variant: InteractiveVariant::Ghost,
            width: None,
            max_height: px(320.0),
            placement: PlacementSpec {
                side: Side::Bottom,
                align: Alignment::Start,
                gap: px(4.0),
                ..PlacementSpec::default()
            },
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
    pub fn width(mut self, width: LogicalPx) -> Self {
        self.width = Some(width);
        self
    }

    #[must_use]
    pub fn max_height(mut self, max_height: LogicalPx) -> Self {
        self.max_height = max_height;
        self
    }

    #[must_use]
    pub fn placement(mut self, placement: PlacementSpec) -> Self {
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
    trigger_width: LogicalPx,
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
    icons: RefCell<IconSnapshot>,
    items: RefCell<Rc<Vec<DropdownItem>>>,
    selected: Signal<Option<usize>>,
    display: Signal<SmolStr>,
    a11y_label: Signal<SmolStr>,
    on_select: RefCell<Option<Rc<dyn Fn(&mut App, SmolStr)>>>,
    actions: DropdownActions,
    watch: RefCell<AnchorWatch>,
    state: RefCell<DropdownState>,
}

impl Dropdown {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        options: DropdownOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        let trigger_click = Action::Custom(format_smolstr!("{key}.trigger"));
        let panel = ItemPanel::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:panel"),
            ItemPanelOptions {
                size: options.size,
                max_height: options.max_height,
                row_height: options.size.h(),
                placement: options.placement.clone(),
                dismiss: DROPDOWN_DISMISS,
                tier: PopupTier::Auto,
                roles: DROPDOWN_ROLES,
                label: options.label.clone(),
                width: if options.width.is_some() {
                    PanelWidth::Exact
                } else {
                    PanelWidth::AtLeast
                },
                typeahead: true,
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
            .a11y_has_popup(HasPopup::Menu)
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
            icons: RefCell::new(IconSnapshot::default()),
            items: RefCell::new(Rc::new(Vec::new())),
            selected: app.signal_scoped(scope, None),
            display: app.signal_scoped(scope, SmolStr::new_static("")),
            a11y_label,
            on_select: RefCell::new(None),
            watch: RefCell::new(AnchorWatch::default()),
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
        self.inner.panel.panel_node()
    }

    #[must_use]
    pub fn popup_node(&self) -> Option<NodeId> {
        self.inner.panel.popup_node()
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
        self.inner
            .icons
            .borrow_mut()
            .resolve(app, window, current_icon.unwrap_or(IconKind::X))?;
        let icon_resource = self
            .inner
            .icons
            .borrow()
            .get(current_icon.unwrap_or(IconKind::X))
            .ok_or(EngineError::UnknownWindow { window })?;
        let icon_slot = Rc::new(std::cell::Cell::new(None));
        let icon_slot_inner = icon_slot.clone();
        let theme_icon = theme.clone();
        let node = trigger.build(app, window, parent, move |app, window, parent| {
            let side = if current_icon.is_some() {
                size.icon_size()
            } else {
                LogicalPx::ZERO
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

    pub fn open(&self, app: &mut App) -> Result<bool, PopupError> {
        let (Some(window), Some(trigger)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().trigger,
        ) else {
            return Ok(false);
        };
        self.measure_trigger(app);
        self.resolve_icons(app, window)
            .map_err(|source| popup_error(app, window, source))?;
        self.inner
            .panel
            .set_content(app, self.panel_content())
            .map_err(|source| popup_error(app, window, source))?;
        let width = self
            .inner
            .options
            .width
            .unwrap_or_else(|| self.inner.state.borrow().trigger_width.max(px(1.0)));
        let opened = self.inner.panel.open(
            app,
            window,
            PanelOpen {
                anchor: Anchor::Node { node: trigger },
                opener: Some(trigger),
                focus: FocusPolicy::Take {
                    restore: RestoreFocus::Opener,
                },
                parent: None,
                width,
                a11y_parent: None,
                surface_action: None,
            },
        )?;
        if opened {
            self.inner.watch.borrow_mut().reset();
            let row = app
                .read_with(&self.inner.selected, |selected| *selected)
                .or_else(|| self.first_enabled());
            self.inner.panel.set_highlighted(app, row);
        }
        Ok(opened)
    }

    pub fn close(&self, app: &mut App) -> bool {
        self.close_with_reason(app, DismissReason::Programmatic)
    }

    fn close_with_reason(&self, app: &mut App, reason: DismissReason) -> bool {
        self.inner.watch.borrow_mut().reset();
        self.inner.panel.close_with_reason(app, reason)
    }

    pub fn close_now(&self, app: &mut App) -> bool {
        self.inner.watch.borrow_mut().reset();
        self.inner.panel.close_now(app)
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.trigger.sync(app)?;
        changed |= self.sync_trigger_a11y(app);
        changed |= self.sync_trigger_icon(app)?;
        if self.is_open(app) {
            let lost = self
                .inner
                .panel
                .id()
                .zip(self.inner.state.borrow().window)
                .is_some_and(|(id, window)| {
                    self.inner.watch.borrow_mut().observe(app, id, window) == AnchorChange::Lost
                });
            if lost {
                changed |= self.close_with_reason(app, DismissReason::AnchorLost);
            }
            self.measure_trigger(app);
            if self.inner.options.width.is_none() {
                let width = self.inner.state.borrow().trigger_width.max(px(1.0));
                changed |= self.inner.panel.set_width(app, width);
            }
        }
        changed |= self.inner.panel.sync(app)?;
        Ok(changed)
    }

    pub fn dispatch(&self, app: &mut App, event: &Event) -> bool {
        self.inner.trigger.dispatch(app, event)
    }

    pub fn release(&self, app: &mut App) -> usize {
        self.inner.watch.borrow_mut().reset();
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
        let icons = Rc::new(self.inner.icons.borrow().clone());
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
            extra: if has_icon {
                size.icon_size()
            } else {
                LogicalPx::ZERO
            },
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
        let items = self.inner.items.borrow().clone();
        let mut kinds: Vec<IconKind> = items.iter().filter_map(|item| item.icon).collect();
        kinds.extend(self.selected_icon(app));
        kinds.sort_unstable();
        kinds.dedup();
        for kind in kinds {
            self.inner.icons.borrow_mut().resolve(app, window, kind)?;
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
            self.inner.icons.borrow_mut().resolve(app, window, kind)?;
            changed = true;
        }
        let resource = desired.and_then(|kind| self.inner.icons.borrow().get(kind));
        if let Some(target) = app.window_mut(window) {
            if let Ok(current) = target.tree().get(node) {
                let mut style = current.style().clone();
                let side = if desired.is_some() {
                    size
                } else {
                    LogicalPx::ZERO
                };
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

    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        a11y::{A11yAction, A11yRole},
        event::{Action, ActionSource, InputEvent, KeyCode, Modifiers, PointerButton},
        interaction::{ScrollConfig, ScrollHandle},
        math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
        paint::style_builder::StyleBuilder,
        popup::{DismissReason, HasPopup, PopupId, PopupSnapshotEntry, PopupState},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{Dropdown, DropdownItem, DropdownOptions};
    use crate::{
        kit::{icon::IconKind, test_support::a11y_snapshot_without_oracle},
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
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
            app.app_mut().on_action(
                Action::Custom(SmolStr::new_static("background.click")),
                move |_| sink.set(sink.get() + 1),
            );
            NodeBuilder::boxed()
                .id("background")
                .layout_override(LogicalRect::new(px(600.0), px(500.0), px(80.0), px(40.0)))
                .on_click(Action::Custom(SmolStr::new_static("background.click")))
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("背景节点");
            Self {
                app,
                window,
                theme,
                selected: Rc::new(RefCell::new(Vec::new())),
                background_clicks: clicks,
            }
        }

        fn dropdown(&mut self, options: DropdownOptions) -> Dropdown {
            let selected = self.selected.clone();
            Dropdown::new(self.app.app_mut(), self.theme.clone(), "dropdown", options)
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
            self.mount_under(dropdown, items, root);
        }

        fn mount_under(
            &mut self,
            dropdown: &Dropdown,
            items: impl IntoIterator<Item = DropdownItem>,
            parent: NodeId,
        ) {
            dropdown.set_items(self.app.app_mut(), items).expect("条目");
            dropdown
                .mount(self.app.app_mut(), self.window, parent)
                .expect("挂载");
            self.settle(dropdown);
        }

        fn scroll_page(&mut self) -> (NodeId, ScrollHandle) {
            let root = self
                .app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .root();
            let page = {
                let tree = self
                    .app
                    .app_mut()
                    .window_mut(self.window)
                    .expect("窗口")
                    .tree_mut();
                NodeBuilder::boxed()
                    .id("page")
                    .style(
                        StyleBuilder::new()
                            .relative()
                            .flex_col()
                            .width(px(400.0))
                            .height(px(200.0))
                            .overflow_hidden()
                            .build(),
                    )
                    .build(tree, root)
                    .expect("滚动页")
            };
            let handle = self
                .app
                .app_mut()
                .make_scroll(self.window, page, ScrollConfig::default())
                .expect("滚动容器");
            self.app
                .app_mut()
                .set_scroll_content(&handle, LogicalSize::new(px(400.0), px(1200.0)))
                .expect("滚动内容");
            (page, handle)
        }

        fn spacer(&mut self, parent: NodeId, height: LogicalPx) {
            let tree = self
                .app
                .app_mut()
                .window_mut(self.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("spacer")
                .style(StyleBuilder::new().width_full().height(height).build())
                .build(tree, parent)
                .expect("占位");
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

        fn drain(&mut self, dropdown: &Dropdown) -> ui_core::test_support::record::FrameRecord {
            let record = self.app.drain().expect("帧").clone();
            let _ = dropdown.sync(self.app.app_mut());
            record
        }

        fn focus_trigger(&mut self, dropdown: &Dropdown) {
            let trigger = dropdown.trigger_node().expect("触发器");
            self.app.app_mut().set_focus(self.window, Some(trigger));
        }

        fn key(&mut self, dropdown: &Dropdown, code: KeyCode) {
            self.key_with(dropdown, code, Modifiers::empty());
        }

        fn key_with(&mut self, dropdown: &Dropdown, code: KeyCode, modifiers: Modifiers) {
            let _ = self.app.inject(InputEvent::key_down(code, modifiers));
            let _ = self.app.inject(InputEvent::key_up(code, modifiers));
            self.drain(dropdown);
        }

        fn mount_focusable(&mut self, id: &'static str, rect: LogicalRect) -> NodeId {
            let root = self
                .app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .root();
            let action = Action::Custom(format_smolstr!("{id}.click"));
            self.app.app_mut().on_action(action.clone(), move |_| {});
            let tree = self
                .app
                .app_mut()
                .window_mut(self.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id(id)
                .tab_stop(true)
                .on_click(action)
                .layout_override(rect)
                .build(tree, root)
                .expect("可聚焦节点")
        }

        fn press_release(&mut self, at: LogicalPoint) {
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
        }

        fn click(&mut self, dropdown: &Dropdown, at: LogicalPoint) {
            self.press_release(at);
            self.settle(dropdown);
        }

        fn bounds(&self, node: NodeId) -> LogicalRect {
            self.app.probe().bounds(node).expect("布局盒")
        }

        fn center(&self, node: NodeId) -> LogicalPoint {
            let bounds = self.bounds(node);
            LogicalPoint::new(
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

        fn row_center(&self, index: usize) -> LogicalPoint {
            self.center(self.row(index))
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

        fn popup_entry(&self, dropdown: &Dropdown) -> PopupSnapshotEntry {
            let node = dropdown.popup_node().expect("浮层内容节点");
            self.app
                .app()
                .popup_stack(self.window)
                .expect("浮层栈")
                .entries
                .into_iter()
                .find(|entry| entry.node == node)
                .expect("面板在栈里")
        }

        fn popup_rect(&self, dropdown: &Dropdown) -> LogicalRect {
            self.popup_entry(dropdown).hit_rect
        }

        fn trigger_screen_rect(&self, dropdown: &Dropdown) -> LogicalRect {
            let trigger = dropdown.trigger_node().expect("触发器");
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .node_screen_rect(trigger)
                .expect("触发器屏幕矩形")
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
            "再点触发器关闭面板（opener Toggle 消费 press）"
        );
        assert_eq!(harness.background_clicks.get(), 0, "opener 按下被消费");
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
    fn an_a11y_click_chooses_the_option_it_targets() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        let row = harness
            .find("dropdown:panel:list:row:3")
            .expect("选项行节点");
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(row).expect("行 a11y 记录");
        assert_eq!(record.role, A11yRole::MenuItem);
        assert!(
            record.actions.contains(&A11yAction::Click),
            "MenuItem 必须宣告 a11y Click：{:?}",
            record.actions
        );

        let action = Action::Custom(SmolStr::new_static("dropdown:panel.row"));
        let handled = harness.app.app_mut().dispatch_action(
            harness.window,
            row,
            action,
            ActionSource::Programmatic,
        );
        assert!(handled, "AT Click 必须被分发到行 action");
        harness.settle(&dropdown);
        assert_eq!(
            harness.selected.borrow().as_slice(),
            &[SmolStr::new_static("d")],
            "AT Click 选中它指向的选项"
        );
        assert!(!dropdown.is_open(harness.app.app()));
    }

    #[test]
    fn a_space_press_also_chooses_the_highlighted_option() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));

        harness.key(&dropdown, KeyCode::End);
        harness.key(&dropdown, KeyCode::Space);
        assert_eq!(
            harness.selected.borrow().as_slice(),
            &[SmolStr::new_static("d")],
            "Space 与 Enter 走同一条激活路径"
        );
        assert!(!dropdown.is_open(harness.app.app()));
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
            (panel.size.width - trigger_width).abs() < px(0.5),
            "面板宽度对齐触发器：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn the_first_visible_frame_already_has_the_settled_rect() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        let trigger_width = harness.bounds(trigger).size.width;
        let _ = harness.app.drain().expect("帧");
        harness.press_release(harness.center(trigger));
        let _ = harness.app.drain().expect("帧");
        let first_entry = harness.popup_entry(&dropdown);
        assert_eq!(
            first_entry.state,
            PopupState::Open,
            "首帧即 Open（不再需要等 reveal）"
        );
        let first = first_entry.hit_rect;
        let first_panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert!(
            (first_panel.size.width - trigger_width).abs() < px(0.5),
            "首帧面板宽度已跟随触发器：面板 {} / 触发器 {trigger_width}",
            first_panel.size.width
        );
        harness.settle(&dropdown);
        let stable = harness.popup_rect(&dropdown);
        assert_eq!(first, stable, "首可见帧 rect 等于稳定帧");
        let stable_panel = harness.bounds(dropdown.panel_node().expect("面板"));
        assert_eq!(
            first_panel.size.width, stable_panel.size.width,
            "首帧面板宽度等于稳定帧"
        );
    }

    #[test]
    fn the_dropdown_slides_in_without_moving_the_settled_rect() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        let _ = harness.app.drain().expect("帧");
        harness.press_release(harness.center(trigger));
        let opening_frame = harness.app.drain().expect("打开帧").clone();
        let opening = harness.popup_entry(&dropdown);
        assert_eq!(
            opening.paint_scale, 1.0,
            "FadeSlide 不做 scale（scale 只属于 FadeScale）"
        );
        assert!(
            opening.paint_offset.x == LogicalPx::ZERO
                && (opening.paint_offset.y - px(4.0)).abs() < px(0.5),
            "Dropdown 必须与 Menu 一致使用 FadeSlide（打开帧向下 4px）：{:?}",
            opening.paint_offset
        );
        assert!(
            !opening_frame.damage_full,
            "slide 打开帧必须局部重绘（I6）：{:?}",
            opening_frame.damage
        );
        let first = opening.hit_rect;
        for _ in 0..3 {
            harness.app.advance(Duration::from_millis(16));
            let record = harness.app.drain().expect("slide 帧").clone();
            assert!(
                !record.damage_full,
                "slide 帧必须局部重绘：{:?}",
                record.damage
            );
        }
        harness.settle(&dropdown);
        let settled = harness.popup_entry(&dropdown);
        assert_eq!(
            settled.paint_offset,
            LogicalPoint::ZERO,
            "动画结束后 paint_offset 必须归零"
        );
        assert_eq!(settled.paint_scale, 1.0);
        assert_eq!(
            settled.hit_rect, first,
            "slide 只改绘制偏移，settle rect 必须首帧即稳定"
        );
        assert_eq!(settled.state, PopupState::Open);
    }

    fn typeahead_items() -> Vec<DropdownItem> {
        vec![
            DropdownItem::new("alpha", "Alpha"),
            DropdownItem::new("beta", "Beta"),
            DropdownItem::new("charlie", "Charlie").disabled(true),
            DropdownItem::new("cobalt", "Cobalt"),
            DropdownItem::new("aurora", "Aurora"),
        ]
    }

    #[test]
    fn tab_closes_the_dropdown_and_moves_focus_past_the_trigger() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let after = harness.mount_focusable(
            "dropdown-after",
            LogicalRect::new(px(0.0), px(450.0), px(80.0), px(32.0)),
        );
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");

        harness.key(&dropdown, KeyCode::Tab);
        assert!(
            !dropdown.is_open(harness.app.app()),
            "Tab 必须离开并关闭非模态面板"
        );
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(after),
            "焦点移到触发器之后的下一个 tab stop"
        );
    }

    #[test]
    fn shift_tab_closes_the_dropdown_and_moves_focus_before_the_trigger() {
        let mut harness = Harness::new();
        let before = harness.mount_focusable(
            "dropdown-before",
            LogicalRect::new(px(0.0), px(100.0), px(80.0), px(32.0)),
        );
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");

        harness.key_with(&dropdown, KeyCode::Tab, Modifiers::SHIFT);
        assert!(
            !dropdown.is_open(harness.app.app()),
            "Shift+Tab 必须离开并关闭面板"
        );
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(before),
            "焦点移到触发器之前的上一个 tab stop"
        );
    }

    #[test]
    fn enter_chooses_the_highlighted_row_and_rows_leave_the_tab_order() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");

        harness.key(&dropdown, KeyCode::ArrowDown);
        harness.key(&dropdown, KeyCode::ArrowDown);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(3),
            "方向键跳过 disabled 行到 Delta"
        );
        harness.key(&dropdown, KeyCode::Enter);
        assert_eq!(
            dropdown.selected(harness.app.app()),
            Some(SmolStr::new_static("d")),
            "Enter 激活的是高亮行 Delta，而不是某个被 Tab 聚焦的行"
        );

        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "再次打开");
        let row = harness
            .find("dropdown:panel:list:row:2")
            .expect("行节点已挂载");
        let _ = harness.app.drain().expect("a11y 帧");
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(row).expect("行 a11y");
        assert!(
            !record.actions.contains(&A11yAction::Focus),
            "列表行不得进入 Tab 序：{:?}",
            record.actions
        );
    }

    #[test]
    fn typeahead_matches_labels_skips_disabled_and_cycles() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, typeahead_items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");

        harness.key(&dropdown, KeyCode::KeyA);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(4),
            "A 从当前行循环到 Aurora"
        );
        harness.key(&dropdown, KeyCode::KeyA);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(0),
            "重复 A 循环回 Alpha"
        );
        harness.app.advance(Duration::from_millis(2100));
        let _ = harness.drain(&dropdown);
        harness.key(&dropdown, KeyCode::KeyC);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(3),
            "C 跳过 disabled 的 Charlie 到 Cobalt"
        );
        harness.app.advance(Duration::from_millis(2100));
        let _ = harness.drain(&dropdown);
        harness.key(&dropdown, KeyCode::KeyC);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(3),
            "C 单候选保持 Cobalt"
        );
        harness.key(&dropdown, KeyCode::KeyO);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(3),
            "CO 前缀继续匹配 Cobalt"
        );
        harness.app.advance(Duration::from_millis(2100));
        let _ = harness.drain(&dropdown);
        harness.key_with(&dropdown, KeyCode::KeyA, Modifiers::SHIFT);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(4),
            "Shift+A 大写输入也按不区分大小写匹配 Aurora"
        );
        assert!(
            dropdown.selected(harness.app.app()).is_none(),
            "typeahead 只移动高亮，不选择"
        );
        assert!(dropdown.is_open(harness.app.app()), "typeahead 不关闭面板");
    }

    #[test]
    fn shift_letter_typeahead_matches_case_insensitively() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, typeahead_items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");

        harness.key_with(&dropdown, KeyCode::KeyA, Modifiers::SHIFT);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(4),
            "Shift+A 必须像 A 一样匹配 Aurora（大小写不敏感）"
        );
        harness.app.advance(Duration::from_millis(2100));
        let _ = harness.drain(&dropdown);
        harness.key_with(&dropdown, KeyCode::KeyB, Modifiers::SHIFT);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(1),
            "Shift+B 匹配 Beta（大写输入和普通输入同路径）"
        );
        assert!(dropdown.is_open(harness.app.app()), "Shift 字母不关闭面板");
    }

    #[test]
    fn typeahead_buffer_expires_after_the_timeout() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, typeahead_items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");

        harness.key(&dropdown, KeyCode::KeyC);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(3),
            "C → Cobalt"
        );
        harness.key(&dropdown, KeyCode::KeyO);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(3),
            "CO → Cobalt"
        );
        harness.app.advance(Duration::from_millis(2100));
        let _ = harness.drain(&dropdown);
        harness.key(&dropdown, KeyCode::KeyA);
        assert_eq!(
            dropdown.highlighted(harness.app.app()),
            Some(4),
            "2s 后缓冲清空：A 单字符从 Cobalt 循环到 Aurora（而非 COA 前缀）"
        );
    }

    #[test]
    fn scrolling_the_trigger_out_of_the_page_closes_the_dropdown() {
        let mut harness = Harness::new();
        let (page, scroll) = harness.scroll_page();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount_under(&dropdown, items(), page);
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");

        harness
            .app
            .app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(600.0)))
            .expect("滚动");
        harness.settle(&dropdown);
        assert!(
            !dropdown.is_open(harness.app.app()),
            "触发器滚出滚动页后关闭面板"
        );
    }

    #[test]
    fn losing_the_anchor_reports_anchor_lost() {
        let mut harness = Harness::new();
        let (page, scroll) = harness.scroll_page();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount_under(&dropdown, items(), page);
        let reasons: Rc<RefCell<Vec<DismissReason>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = reasons.clone();
        dropdown
            .inner
            .panel
            .on_close(move |_, reason| sink.borrow_mut().push(reason));
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");

        harness
            .app
            .app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(600.0)))
            .expect("滚动");
        harness.settle(&dropdown);
        assert!(
            !dropdown.is_open(harness.app.app()),
            "锚点滚出视口后关闭面板"
        );
        assert_eq!(
            reasons.borrow().as_slice(),
            &[DismissReason::AnchorLost],
            "锚点丢失/滚出视口关闭必须上报 AnchorLost（R2-02/Q2-07）"
        );
    }

    #[test]
    fn scrolling_a_visible_trigger_re_anchors_the_open_dropdown() {
        let mut harness = Harness::new();
        let (page, scroll) = harness.scroll_page();
        harness.spacer(page, px(100.0));
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount_under(&dropdown, items(), page);
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()), "面板打开");
        let before = harness.popup_rect(&dropdown);

        harness
            .app
            .app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(60.0)))
            .expect("滚动");
        harness.settle(&dropdown);
        let after_trigger = harness.trigger_screen_rect(&dropdown);
        assert!(
            dropdown.is_open(harness.app.app()),
            "触发器仍可见时保持打开：滚动前浮层 {before:?} / 滚动后触发器 {after_trigger:?}"
        );
        let after = harness.popup_rect(&dropdown);
        assert_ne!(before, after, "滚动后浮层随锚点重新落位");
        let trigger_rect = harness.trigger_screen_rect(&dropdown);
        assert!(
            (after.origin.y - (trigger_rect.origin.y + trigger_rect.size.height + px(4.0))).abs()
                < px(0.5),
            "浮层贴住触发器下沿：浮层 {after:?} / 触发器 {trigger_rect:?}"
        );
    }

    #[test]
    fn a_long_item_list_is_virtualized_and_scrolls_to_the_end() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions {
            max_height: px(200.0),
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
            panel.size.height <= px(200.0 + 0.5),
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
    fn a_press_outside_closes_the_panel_and_replays_the_press_to_the_page() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()));

        harness.click(&dropdown, LogicalPoint::new(px(640.0), px(520.0)));
        assert!(!dropdown.is_open(harness.app.app()), "点击外部关闭");
        assert_eq!(
            harness.background_clicks.get(),
            1,
            "D14：外部按下按 Replay 重派给下层命中控件"
        );
    }

    #[test]
    fn escape_closes_the_panel_and_returns_focus_to_the_trigger() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        harness.key(&dropdown, KeyCode::Escape);
        assert!(!dropdown.is_open(harness.app.app()), "Esc 关闭面板");
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger),
            "关闭后焦点回到 opener"
        );
    }

    #[test]
    fn a_closing_dropdown_reopens_when_its_trigger_is_pressed_again() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        let center = harness.center(trigger);
        harness.click(&dropdown, center);
        harness.key(&dropdown, KeyCode::Escape);
        assert!(!dropdown.is_open(harness.app.app()));

        let _ = harness.app.inject(InputEvent::pointer_down(
            center,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        let _ = dropdown.sync(harness.app.app_mut());
        assert!(
            dropdown.is_open(harness.app.app()),
            "Closing 中按 opener 由引擎取消关闭并回到 Opening（Toggle 消费）"
        );
    }

    #[test]
    fn a_closed_dropdown_does_not_swallow_a_click_behind_it() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        harness.click(&dropdown, LogicalPoint::new(px(640.0), px(520.0)));
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
        let wrapper = dropdown.popup_node().expect("浮层内容节点");
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
            "长条目把面板撑宽：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn an_explicit_width_stays_exact_for_long_items() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default().width(px(120.0)));
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
            (panel.size.width - 120.0).abs() < px(0.5),
            "显式宽度精确生效、不随长条目增长：{}",
            panel.size.width
        );
        let row = harness.bounds(harness.row(0));
        assert!(
            row.size.width <= px(120.0 - 8.0 + 0.5),
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
        assert_eq!(
            record.has_popup,
            Some(HasPopup::Menu),
            "关闭态静态 aria-haspopup=menu"
        );
        assert_eq!(record.expanded, Some(false), "关闭态 aria-expanded=false");

        harness.click(&dropdown, harness.center(trigger));
        harness.settle(&dropdown);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(
            record.has_popup,
            Some(HasPopup::Menu),
            "打开态 haspopup 仍保留"
        );
        assert_eq!(record.expanded, Some(true), "打开态 aria-expanded=true");

        harness.click(&dropdown, harness.row_center(1));
        harness.settle(&dropdown);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.label_text(), Some("Beta"), "可访问名跟随选中项");
        assert_eq!(
            record.has_popup,
            Some(HasPopup::Menu),
            "关闭后静态 haspopup 不丢失"
        );
        assert_eq!(record.expanded, Some(false), "关闭后 expanded 复原");
    }

    #[test]
    fn an_open_dropdown_does_not_swallow_background_scroll() {
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
                        .width(px(800.0))
                        .height(px(600.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("页面")
        };
        let handle = harness
            .app
            .app_mut()
            .make_scroll(harness.window, page, ScrollConfig::default())
            .expect("滚动容器");
        harness
            .app
            .app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(800.0), px(2400.0)))
            .expect("内容");

        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()));

        let wheel = |harness: &mut Harness, dropdown: &Dropdown| {
            let _ = harness.app.inject(InputEvent::scroll_lines(
                LogicalPoint::new(px(700.0), px(500.0)),
                LogicalPoint::new(px(0.0), px(-2.0)),
                Modifiers::empty(),
            ));
            harness.settle(dropdown);
        };
        wheel(&mut harness, &dropdown);
        assert!(
            harness
                .app
                .app()
                .scroll_state(&handle)
                .expect("滚动状态")
                .offset
                .y
                > px(50.0),
            "Auto 非模态下拉不吞背景滚动"
        );

        harness.key(&dropdown, KeyCode::Escape);
        harness.settle(&dropdown);
    }

    #[test]
    fn an_open_dropdown_follows_its_scrolled_trigger() {
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
                        .width(px(700.0))
                        .height(px(400.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("页面")
        };
        let handle = harness
            .app
            .app_mut()
            .make_scroll(harness.window, page, ScrollConfig::default())
            .expect("滚动容器");
        harness
            .app
            .app_mut()
            .set_scroll_content(&handle, LogicalSize::new(px(700.0), px(1600.0)))
            .expect("内容");
        let content = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("page-content")
                .style(
                    StyleBuilder::new()
                        .width_full()
                        .height(px(1600.0))
                        .flex_col()
                        .padding_y(px(300.0))
                        .build(),
                )
                .build(tree, page)
                .expect("内容节点")
        };
        let dropdown = harness.dropdown(DropdownOptions::default().width(px(120.0)));
        harness.mount_under(&dropdown, items(), content);
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        assert!(dropdown.is_open(harness.app.app()));

        let before = harness.popup_rect(&dropdown);
        let _ = harness.app.inject(InputEvent::scroll_lines(
            LogicalPoint::new(px(500.0), px(200.0)),
            LogicalPoint::new(px(0.0), px(-2.0)),
            Modifiers::empty(),
        ));
        harness.settle(&dropdown);

        let trigger_rect = harness.trigger_screen_rect(&dropdown);
        let after = harness.popup_rect(&dropdown);
        assert!(
            after.origin.y < before.origin.y - px(10.0),
            "触发器滚动后面板同帧跟随：before={before:?} after={after:?}"
        );
        let expected = trigger_rect.max().y + px(4.0);
        assert!(
            (after.origin.y - expected).value().abs() < 0.5,
            "面板顶边贴触发器底边：expected {expected} actual {}",
            after.origin.y
        );
    }

    #[test]
    fn opening_and_closing_the_dropdown_never_forces_a_full_repaint() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        let id: PopupId = harness.popup_entry(&dropdown).id;
        for _ in 0..4 {
            let record = harness.drain(&dropdown);
            assert!(!record.damage_full, "打开面板不得全量重绘");
        }
        assert!(dropdown.close(harness.app.app_mut()));
        for _ in 0..12 {
            harness.app.advance(Duration::from_millis(16));
            let record = harness.drain(&dropdown);
            assert!(!record.damage_full, "关闭面板不得全量重绘");
            if harness.app.app().popup_rect(id).is_none() {
                break;
            }
        }
        assert!(
            harness.app.app().popup_rect(id).is_none(),
            "关闭动画结束后节点卸载"
        );
    }

    #[test]
    fn the_panel_debug_ids_stay_stable() {
        let mut harness = Harness::new();
        let dropdown = harness.dropdown(DropdownOptions::default());
        harness.mount(&dropdown, items());
        let trigger = dropdown.trigger_node().expect("触发器");
        harness.click(&dropdown, harness.center(trigger));
        let panel = dropdown.panel_node().expect("面板");
        assert_eq!(harness.find("dropdown:panel:panel"), Some(panel));
        assert!(harness.find("dropdown:panel:surface").is_some());
        assert!(
            harness.find("dropdown:panel:list:viewport").is_some(),
            "列表 viewport 命名保持（menu_metrics 依赖）"
        );
    }
}
