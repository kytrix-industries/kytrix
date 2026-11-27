use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::Display;
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::App,
    error::EngineError,
    event::{Action, Event, KeyBinding, KeyCode, Modifiers},
    interaction::ActionCtx,
    math::{LogicalPx, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        image::ImageFit,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{
        primitive::Border,
        style::StylePatch,
        style_builder::{PaintBuilder, StyleBuilder},
    },
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

const TAG_RADIUS: LogicalPx = LogicalPx::new(4.0);
const TAG_GAP: LogicalPx = LogicalPx::new(4.0);
const CHECK_COLUMN: LogicalPx = LogicalPx::new(14.0);
const SELECT_DISMISS: DismissPolicy = DismissPolicy::Chain {
    outside: OutsidePress::Replay,
    escape: EscapePolicy::CloseTopmost,
    anchor_lost: true,
    focus_lost: true,
};
const SELECT_ROLES: PanelRoles = PanelRoles {
    panel: None,
    row: A11yRole::ListBoxOption,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectItem {
    pub id: SmolStr,
    pub label: SmolStr,
    pub icon: Option<IconKind>,
    pub disabled: bool,
}

impl SelectItem {
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
pub struct SelectOptions {
    pub size: UnifiedSize,
    pub variant: InteractiveVariant,
    pub width: Option<LogicalPx>,
    pub max_height: LogicalPx,
    pub placement: PlacementSpec,
    pub label: Option<SmolStr>,
    pub placeholder: SmolStr,
    pub multiple: bool,
    pub clearable: bool,
    pub max_tags: usize,
}

impl Default for SelectOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            variant: InteractiveVariant::Outline,
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
            multiple: false,
            clearable: false,
            max_tags: 2,
        }
    }
}

impl SelectOptions {
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

    #[must_use]
    pub fn multiple(mut self, multiple: bool) -> Self {
        self.multiple = multiple;
        self
    }

    #[must_use]
    pub fn clearable(mut self, clearable: bool) -> Self {
        self.clearable = clearable;
        self
    }

    #[must_use]
    pub fn max_tags(mut self, max_tags: usize) -> Self {
        self.max_tags = max_tags.max(1);
        self
    }
}

#[derive(Clone)]
pub struct Select {
    inner: Rc<SelectInner>,
}

#[derive(Default)]
struct SelectState {
    window: Option<WindowId>,
    trigger: Option<NodeId>,
    trigger_width: LogicalPx,
    tags_written: Vec<usize>,
    slots: Vec<Option<NodeId>>,
    overflow: Option<NodeId>,
    trigger_icon: Option<NodeId>,
    icon_shown: Option<IconKind>,
}

struct SelectActions {
    trigger_click: Action,
    down: Action,
    up: Action,
    clear: Action,
}

struct SelectInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: SelectOptions,
    scope: SignalScope,
    trigger: Interactive,
    panel: ItemPanel,
    icons: RefCell<IconSnapshot>,
    items: Rc<RefCell<Rc<Vec<SelectItem>>>>,
    revision: Signal<u64>,
    chosen: Signal<Vec<usize>>,
    a11y_label: Signal<SmolStr>,
    tooltip: RefCell<Option<SmolStr>>,
    on_change: RefCell<Option<Rc<dyn Fn(&mut App, Vec<SmolStr>)>>>,
    actions: SelectActions,
    watch: RefCell<AnchorWatch>,
    state: RefCell<SelectState>,
}

impl Select {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        options: SelectOptions,
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
                dismiss: SELECT_DISMISS,
                tier: PopupTier::Auto,
                roles: SELECT_ROLES,
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
            .a11y_has_popup(HasPopup::ListBox)
            .action(trigger_click.clone());
        let inner = Rc::new(SelectInner {
            actions: SelectActions {
                trigger_click,
                down: Action::Custom(format_smolstr!("{key}.open-down")),
                up: Action::Custom(format_smolstr!("{key}.open-up")),
                clear: Action::Custom(format_smolstr!("{key}.clear")),
            },
            key,
            theme,
            options,
            scope,
            trigger,
            panel,
            icons: RefCell::new(IconSnapshot::default()),
            items: Rc::new(RefCell::new(Rc::new(Vec::new()))),
            revision: app.signal_scoped(scope, 0u64),
            chosen: app.signal_scoped(scope, Vec::new()),
            a11y_label,
            tooltip: RefCell::new(None),
            on_change: RefCell::new(None),
            watch: RefCell::new(AnchorWatch::default()),
            state: RefCell::new(SelectState::default()),
        });
        let select = Self { inner };
        select.install_handlers(app);
        select
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
    pub fn chosen(&self, app: &App) -> Vec<SmolStr> {
        let items = self.inner.items.borrow();
        app.read_with(&self.inner.chosen, |chosen| {
            chosen
                .iter()
                .filter_map(|index| items.get(*index).map(|item| item.id.clone()))
                .collect()
        })
    }

    #[must_use]
    pub fn chosen_indices(&self, app: &App) -> Vec<usize> {
        app.read_with(&self.inner.chosen, Vec::clone)
    }

    #[must_use]
    pub fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.panel.highlighted(app)
    }

    #[must_use]
    pub fn items(&self) -> Rc<Vec<SelectItem>> {
        self.inner.items.borrow().clone()
    }

    #[must_use]
    pub fn placeholder_text(&self) -> SmolStr {
        self.inner.options.placeholder.clone()
    }

    pub fn set_items(
        &self,
        app: &mut App,
        items: impl IntoIterator<Item = SelectItem>,
    ) -> Result<(), EngineError> {
        let previous = self.chosen(app);
        *self.inner.items.borrow_mut() = Rc::new(items.into_iter().collect());
        app.write(
            &self.inner.revision,
            app.read_with(&self.inner.revision, |revision| *revision) + 1,
        );
        let chosen: Vec<usize> = previous.iter().filter_map(|id| self.index_of(id)).collect();
        app.write(&self.inner.chosen, chosen);
        if let Some(window) = self.inner.state.borrow().window {
            self.resolve_icons(app, window)?;
        }
        self.sync_trigger_a11y(app);
        self.inner.panel.set_content(app, self.panel_content(app))
    }

    pub fn set_chosen<'a>(
        &self,
        app: &mut App,
        ids: impl IntoIterator<Item = &'a str>,
        notify: bool,
    ) {
        let mut chosen: Vec<usize> = ids.into_iter().filter_map(|id| self.index_of(id)).collect();
        chosen.sort_unstable();
        chosen.dedup();
        if !self.inner.options.multiple {
            chosen.truncate(1);
        }
        app.write(&self.inner.chosen, chosen);
        self.sync_trigger_a11y(app);
        if notify {
            self.notify(app);
        }
    }

    pub fn clear(&self, app: &mut App) -> bool {
        if !app.write(&self.inner.chosen, Vec::new()) {
            return false;
        }
        self.sync_trigger_a11y(app);
        self.notify(app);
        true
    }

    pub fn on_change(self, callback: impl Fn(&mut App, Vec<SmolStr>) + 'static) -> Self {
        *self.inner.on_change.borrow_mut() = Some(Rc::new(callback));
        self
    }

    pub fn tooltip_text(self, text: impl Into<SmolStr>) -> Self {
        *self.inner.tooltip.borrow_mut() = Some(text.into());
        self
    }

    #[must_use]
    pub fn tooltip(&self) -> Option<SmolStr> {
        self.inner.tooltip.borrow().clone()
    }

    pub fn set_disabled(&self, app: &mut App, disabled: bool) -> bool {
        self.inner.trigger.set_disabled(app, disabled)
    }

    #[must_use]
    pub fn is_disabled(&self, app: &App) -> bool {
        *app.read(&self.inner.trigger.disabled_signal())
    }

    pub fn build(
        &self,
        app: &mut App,
        window: WindowId,
        parent: NodeId,
    ) -> Result<NodeId, EngineError> {
        self.inner.state.borrow_mut().window = Some(window);
        let size = self.inner.options.size;
        let multiple = self.inner.options.multiple;
        let clearable = self.inner.options.clearable;
        let placeholder = self.inner.options.placeholder.clone();
        let chosen = self.inner.chosen;
        let revision = self.inner.revision;
        let theme = self.inner.theme.clone();
        let trigger = self.inner.trigger.clone();
        let items = self.inner.items.clone();
        let slots_sink = Rc::new(RefCell::new(Vec::new()));
        let slots = slots_sink.clone();
        let overflow_sink = Rc::new(Cell::new(None));
        let overflow = overflow_sink.clone();
        let max_tags = self.inner.options.max_tags.max(1);
        let key = self.inner.key.clone();
        let clear_action = self.inner.actions.clear.clone();
        let trigger_watch = trigger.clone();
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
        let icon_slot = Rc::new(Cell::new(None));
        let icon_slot_inner = icon_slot.clone();
        let node = trigger.build(app, window, parent, move |app, window, parent| {
            let side = if current_icon.is_some() {
                size.icon_size()
            } else {
                LogicalPx::ZERO
            };
            let theme_icon = theme.clone();
            let icon = NodeBuilder::image(icon_resource, ImageFit::Fill)
                .style(StyleBuilder::new().size(side, side).build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_icon.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .a11y(A11yOverride {
                    hidden: true,
                    ..A11yOverride::default()
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    parent,
                )?;
            icon_slot_inner.set(Some(icon));
            let theme_text = theme.clone();
            let text_placeholder = placeholder.clone();
            let text = NodeBuilder::text(
                SmolStr::new_static(""),
                TextStyle::new(size.text_size())
                    .with_wrap(TextWrap::NoWrap)
                    .with_overflow(TextOverflow::Ellipsis, 1),
            )
            .style(StyleBuilder::new().flex_1().min_width_0().build())
            .bind_text({
                let items = items.clone();
                move |cx| {
                    let _ = *cx.read(&revision);
                    let chosen = cx.read(&chosen);
                    let items = items.borrow();
                    if multiple {
                        if chosen.is_empty() {
                            text_placeholder.clone()
                        } else {
                            SmolStr::new_static("")
                        }
                    } else {
                        chosen
                            .first()
                            .and_then(|index| items.get(*index))
                            .map(|item| item.label.clone())
                            .unwrap_or_else(|| text_placeholder.clone())
                    }
                }
            })
            .bind_style(move |cx| {
                let empty = cx.read(&chosen).is_empty();
                StylePatch {
                    foreground: Some(theme_text.role(
                        cx,
                        if empty {
                            Role::FgMuted
                        } else {
                            Role::FgDefault
                        },
                    )),
                    ..StylePatch::default()
                }
            })
            .build(
                app.window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut(),
                parent,
            )?;
            trigger_watch.watch(text);
            if multiple {
                let row = NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .flex_row()
                            .items_center()
                            .relative()
                            .flex_1()
                            .min_width_0()
                            .gap(TAG_GAP)
                            .overflow_hidden()
                            .build(),
                    )
                    .a11y(A11yOverride {
                        hidden: true,
                        ..A11yOverride::default()
                    })
                    .build(
                        app.window_mut(window)
                            .ok_or(EngineError::UnknownWindow { window })?
                            .tree_mut(),
                        parent,
                    )?;
                let mut built = Vec::with_capacity(max_tags);
                for slot in 0..max_tags {
                    let chip = build_tag_chip(
                        app,
                        window,
                        TagChip {
                            parent: row,
                            size,
                            theme: &theme,
                            chosen,
                            revision,
                            items: &items,
                            key: &key,
                        },
                        slot,
                    )?;
                    built.push(Some(chip));
                }
                *slots.borrow_mut() = built;
                let theme_overflow = theme.clone();
                let overflow_text = NodeBuilder::text(
                    SmolStr::new_static(""),
                    TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
                )
                .style(hidden_style(StyleBuilder::new().flex_none().build()))
                .bind_text({
                    let items = items.clone();
                    move |cx| {
                        let _ = *cx.read(&revision);
                        let chosen = cx.read(&chosen);
                        let shown = chosen.len().min(max_tags);
                        if chosen.len() > shown {
                            let items = items.borrow();
                            let mut overflow = 0usize;
                            for index in &chosen[shown..] {
                                if items.get(*index).is_some() {
                                    overflow += 1;
                                }
                            }
                            format_smolstr!("+{overflow}")
                        } else {
                            SmolStr::new_static("")
                        }
                    }
                })
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_overflow.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(
                    app.window_mut(window)
                        .ok_or(EngineError::UnknownWindow { window })?
                        .tree_mut(),
                    row,
                )?;
                overflow.set(Some(overflow_text));
            }
            if clearable {
                let theme_clear = theme.clone();
                let clear = NodeBuilder::boxed()
                    .style(
                        StyleBuilder::new()
                            .size(size.icon_size(), size.icon_size())
                            .build(),
                    )
                    .a11y(A11yOverride {
                        role: Some(A11yRole::Button),
                        label: Some(SmolStr::new_static("Clear")),
                        ..A11yOverride::default()
                    })
                    .on_click(clear_action.clone())
                    .bind_style(move |cx| StylePatch {
                        visible: Some(!cx.read(&chosen).is_empty()),
                        foreground: Some(theme_clear.role(cx, Role::FgMuted)),
                        ..StylePatch::default()
                    })
                    .build(
                        app.window_mut(window)
                            .ok_or(EngineError::UnknownWindow { window })?
                            .tree_mut(),
                        parent,
                    )?;
                let theme_icon = theme.clone();
                let tree = app
                    .window_mut(window)
                    .ok_or(EngineError::UnknownWindow { window })?
                    .tree_mut();
                let icon_style = StyleBuilder::new()
                    .size(size.icon_size(), size.icon_size())
                    .build();
                NodeBuilder::text(
                    SmolStr::new_static("×"),
                    TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
                )
                .style(icon_style)
                .a11y(A11yOverride {
                    hidden: true,
                    ..A11yOverride::default()
                })
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_icon.role(cx, Role::FgMuted)),
                    ..StylePatch::default()
                })
                .build(tree, clear)?;
            }
            let theme_caret = theme.clone();
            mount_dynamic_icon(
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
            Ok(())
        })?;
        {
            let mut state = self.inner.state.borrow_mut();
            state.trigger = Some(node);
            state.slots = slots_sink.borrow().clone();
            state.overflow = overflow_sink.get();
            state.trigger_icon = icon_slot.get();
            state.icon_shown = current_icon;
        }
        self.sync_trigger_a11y(app);
        self.sync_tags(app);
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
        let width = self
            .inner
            .options
            .width
            .unwrap_or_else(|| self.inner.state.borrow().trigger_width.max(px(1.0)));
        self.inner
            .panel
            .set_content(app, self.panel_content(app))
            .map_err(|source| popup_error(app, window, source))?;
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
            let row = self
                .chosen_indices(app)
                .first()
                .copied()
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
        changed |= self.sync_tags(app);
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
            &self.inner.actions.clear,
        ] {
            app.off_action(action);
        }
        let released = self.inner.trigger.release(app) + self.inner.panel.release(app);
        released + app.release_scope(self.inner.scope)
    }

    fn panel_content(&self, app: &App) -> PanelContent {
        let items = self.inner.items.borrow().clone();
        let chosen_snapshot = self.chosen_indices(app);
        let rows: Vec<PanelRow> = items
            .iter()
            .map(|item| PanelRow {
                height: self.inner.options.size.h(),
                disabled: item.disabled,
            })
            .collect();
        let theme = self.inner.theme.clone();
        let size = self.inner.options.size;
        let chosen = self.inner.chosen;
        let labels: Vec<SmolStr> = items.iter().map(|item| item.label.clone()).collect();
        let icons = Rc::new(self.inner.icons.borrow().clone());
        let items_build = items.clone();
        let has_icon = items.iter().any(|item| item.icon.is_some());
        let build: Rc<dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>> = {
            let labels = labels.clone();
            Rc::new(move |tree, parent, index| {
                let Some(label) = labels.get(index) else {
                    return Ok(());
                };
                let icon = items_build.get(index).and_then(|item| item.icon);
                build_icon_slot(tree, parent, &theme, &icons, icon, size, has_icon)?;
                let theme_text = theme.clone();
                NodeBuilder::text(
                    label.clone(),
                    TextStyle::new(size.text_size())
                        .with_wrap(TextWrap::NoWrap)
                        .with_overflow(TextOverflow::Ellipsis, 1),
                )
                .style(StyleBuilder::new().flex_1().min_width_0().build())
                .bind_style(move |cx| StylePatch {
                    foreground: Some(theme_text.role(cx, Role::FgDefault)),
                    ..StylePatch::default()
                })
                .build(tree, parent)?;
                let theme_check = theme.clone();
                NodeBuilder::text(
                    SmolStr::new_static("✓"),
                    TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
                )
                .style(StyleBuilder::new().width(CHECK_COLUMN).flex_none().build())
                .bind_style(move |cx| StylePatch {
                    visible: Some(cx.read(&chosen).contains(&index)),
                    foreground: Some(theme_check.role(cx, Role::FgDefault)),
                    ..StylePatch::default()
                })
                .build(tree, parent)
                .map(|_| ())
            })
        };
        let a11y_labels = labels.clone();
        let a11y_chosen = chosen_snapshot;
        let disabled: Vec<bool> = items.iter().map(|item| item.disabled).collect();
        let a11y: Rc<dyn Fn(usize) -> A11yPatch> =
            Rc::new(move |index| match a11y_labels.get(index) {
                Some(label) => A11yPatch {
                    role: Some(A11yRole::ListBoxOption),
                    label: Some(label.clone()),
                    disabled: disabled.get(index).copied(),
                    selected: Some(a11y_chosen.contains(&index)),
                    checked: Some(a11y_chosen.contains(&index)),
                    ..A11yPatch::default()
                },
                None => A11yPatch {
                    hidden: Some(true),
                    ..A11yPatch::default()
                },
            });
        let extra = CHECK_COLUMN
            + if has_icon {
                size.icon_size()
            } else {
                LogicalPx::ZERO
            };
        let measure = Some(PanelMeasure { labels, extra });
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
            let row = this
                .chosen_indices(app)
                .first()
                .copied()
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
        app.on_action(self.inner.actions.clear.clone(), move |ctx| {
            let app = ctx.app_mut();
            if this.clear(app) {
                let _ = this.inner.panel.close(app);
            }
        });
        let this = self.clone();
        self.inner.panel.on_activate(move |app, row| {
            this.activate_row(app, row);
        });
    }

    fn activate_row(&self, app: &mut App, row: usize) {
        let item = self.inner.items.borrow().get(row).cloned();
        let Some(item) = item else {
            return;
        };
        if item.disabled {
            return;
        }
        let multiple = self.inner.options.multiple;
        let mut chosen = self.chosen_indices(app);
        if multiple {
            if let Some(position) = chosen.iter().position(|index| *index == row) {
                chosen.remove(position);
            } else {
                chosen.push(row);
            }
            chosen.sort_unstable();
            app.write(&self.inner.chosen, chosen);
        } else {
            let unchanged = chosen.as_slice() == [row];
            app.write(&self.inner.chosen, vec![row]);
            self.sync_trigger_a11y(app);
            let _ = self.close(app);
            if !unchanged {
                self.notify(app);
            }
            return;
        }
        let _ = self.sync_tags(app);
        self.sync_trigger_a11y(app);
        self.notify(app);
    }

    fn sync_trigger_a11y(&self, app: &mut App) -> bool {
        if self.inner.options.label.is_some() {
            return false;
        }
        let next = {
            let indices = self.chosen_indices(app);
            let items = self.inner.items.borrow();
            match indices.split_first() {
                Some((first, rest)) => {
                    let label = items
                        .get(*first)
                        .map(|item| item.label.clone())
                        .unwrap_or_default();
                    if rest.is_empty() {
                        label
                    } else {
                        format_smolstr!("{label} +{}", rest.len())
                    }
                }
                None => self.inner.options.placeholder.clone(),
            }
        };
        app.write(&self.inner.a11y_label, next)
    }

    fn selected_icon(&self, app: &App) -> Option<IconKind> {
        let indices = self.chosen_indices(app);
        let first = *indices.first()?;
        self.inner
            .items
            .borrow()
            .get(first)
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

    fn notify(&self, app: &mut App) {
        if let Some(callback) = self.inner.on_change.borrow().clone() {
            let chosen = self.chosen(app);
            callback(app, chosen);
        }
    }

    fn sync_tags(&self, app: &mut App) -> bool {
        let multiple = self.inner.options.multiple;
        if !multiple {
            return false;
        }
        let chosen = self.chosen_indices(app);
        let mut state = self.inner.state.borrow_mut();
        if state.tags_written == chosen {
            return false;
        }
        let Some(window) = state.window else {
            return false;
        };
        let max_tags = self.inner.options.max_tags.max(1);
        let slots = state.slots.clone();
        let overflow = state.overflow;
        state.tags_written = chosen.clone();
        drop(state);
        let Some(tree) = app.window_mut(window).map(|target| target.tree_mut()) else {
            return false;
        };
        for (slot, node) in slots.iter().enumerate() {
            let Some(node) = node else {
                continue;
            };
            let display = if slot < chosen.len().min(max_tags) {
                Display::Flex
            } else {
                Display::None
            };
            set_display(tree, *node, display);
        }
        if let Some(overflow) = overflow {
            set_display(
                tree,
                overflow,
                if chosen.len() > max_tags {
                    Display::Flex
                } else {
                    Display::None
                },
            );
        }
        true
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

fn set_display(tree: &mut NodeTree, node: NodeId, display: Display) {
    if let Ok(target) = tree.get(node) {
        let mut style = target.style().clone();
        if style.display_mode() == display {
            return;
        }
        style.display(display);
        let _ = tree.set_style(node, style);
    }
}

struct TagChip<'a> {
    parent: NodeId,
    size: UnifiedSize,
    theme: &'a ThemeHandle,
    chosen: Signal<Vec<usize>>,
    revision: Signal<u64>,
    items: &'a Rc<RefCell<Rc<Vec<SelectItem>>>>,
    key: &'a str,
}

fn build_tag_chip(
    app: &mut App,
    window: WindowId,
    chip: TagChip<'_>,
    slot: usize,
) -> Result<NodeId, EngineError> {
    let TagChip {
        parent,
        size,
        theme,
        chosen,
        revision,
        items,
        key,
    } = chip;
    let chip_height = (size.h() - 8.0).max(px(16.0));
    let theme_chip = theme.clone();
    let tree = app
        .window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .tree_mut();
    let chip = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:tag:{slot}"))
        .style(hidden_style(
            StyleBuilder::new()
                .flex_row()
                .items_center()
                .height(chip_height)
                .padding_x(size.px() / 2.0)
                .border(px(1.0))
                .build(),
        ))
        .paint(PaintBuilder::new().rounded(TAG_RADIUS).build())
        .bind_style(move |cx| StylePatch {
            background: Some(theme_chip.role(cx, Role::SurfaceControl)),
            foreground: Some(theme_chip.role(cx, Role::FgDefault)),
            border: Some(Border {
                width: px(1.0),
                color: theme_chip.role(cx, Role::LineBorder),
            }),
            ..StylePatch::default()
        })
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)?;
    let theme_text = theme.clone();
    let items = items.clone();
    NodeBuilder::text(
        SmolStr::new_static(""),
        TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
    )
    .bind_text(move |cx| {
        let _ = *cx.read(&revision);
        let chosen = cx.read(&chosen);
        let items = items.borrow();
        chosen
            .get(slot)
            .and_then(|index| items.get(*index))
            .map(|item| item.label.clone())
            .unwrap_or_default()
    })
    .bind_style(move |cx| StylePatch {
        foreground: Some(theme_text.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(
        app.window_mut(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .tree_mut(),
        chip,
    )?;
    Ok(chip)
}

fn hidden_style(mut style: ui_core::paint::style::Style) -> ui_core::paint::style::Style {
    style.display(Display::None);
    style
}

impl fmt::Debug for Select {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Select")
            .field("key", &self.inner.key)
            .finish()
    }
}
#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc, time::Duration};

    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        a11y::{A11yAction, A11yRole},
        event::{Action, ActionSource, Event, InputEvent, KeyCode, Modifiers, PointerButton},
        interaction::{ScrollConfig, ScrollHandle},
        math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, tree::NodeKind},
        paint::style_builder::StyleBuilder,
        popup::{DismissReason, HasPopup, PopupId, PopupSnapshotEntry, PopupState},
        test_support::{app::TestApp, diagnostics::signal_slot_stats},
        window::WindowId,
    };

    use super::{Select, SelectItem, SelectOptions};
    use crate::{
        kit::{icon::IconKind, test_support::a11y_snapshot_without_oracle},
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        changed: Rc<RefCell<Vec<Vec<SmolStr>>>>,
        background_clicks: Rc<std::cell::Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let clicks = Rc::new(std::cell::Cell::new(0));
            let sink = clicks.clone();
            app.app_mut().on_action(
                Action::Custom(SmolStr::new_static("background.click")),
                move |_| sink.set(sink.get() + 1),
            );
            NodeBuilder::boxed()
                .id("background")
                .layout_override(LogicalRect::new(px(620.0), px(520.0), px(80.0), px(40.0)))
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
                changed: Rc::new(RefCell::new(Vec::new())),
                background_clicks: clicks,
            }
        }

        fn select(&mut self, options: SelectOptions) -> Select {
            let changed = self.changed.clone();
            Select::new(self.app.app_mut(), self.theme.clone(), "select", options)
                .on_change(move |_, chosen| changed.borrow_mut().push(chosen))
        }

        fn mount(&mut self, select: &Select, items: impl IntoIterator<Item = SelectItem>) {
            let root = self
                .app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .root();
            self.mount_under(select, items, root);
        }

        fn mount_under(
            &mut self,
            select: &Select,
            items: impl IntoIterator<Item = SelectItem>,
            parent: NodeId,
        ) {
            select.set_items(self.app.app_mut(), items).expect("条目");
            select
                .mount(self.app.app_mut(), self.window, parent)
                .expect("挂载");
            self.settle(select);
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

        fn settle(&mut self, select: &Select) {
            for _ in 0..6 {
                self.app.advance(Duration::from_millis(40));
                let _ = self.app.drain().expect("帧");
                let _ = select.sync(self.app.app_mut());
            }
            self.app.advance(Duration::from_millis(200));
            let _ = self.app.drain().expect("帧");
            let _ = select.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
        }

        fn drain(&mut self, select: &Select) -> ui_core::test_support::record::FrameRecord {
            let record = self.app.drain().expect("帧").clone();
            let _ = select.sync(self.app.app_mut());
            record
        }

        fn focus_trigger(&mut self, select: &Select) {
            let trigger = select.trigger_node().expect("触发器");
            self.app.app_mut().set_focus(self.window, Some(trigger));
        }

        fn key(&mut self, select: &Select, code: KeyCode) {
            self.key_with(select, code, Modifiers::empty());
        }

        fn key_with(&mut self, select: &Select, code: KeyCode, modifiers: Modifiers) {
            let _ = self.app.inject(InputEvent::key_down(code, modifiers));
            let _ = self.app.inject(InputEvent::key_up(code, modifiers));
            self.settle(select);
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

        fn click(&mut self, select: &Select, at: LogicalPoint) {
            self.press_release(at);
            self.settle(select);
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
            self.find(&format!("select:panel:list:row:{index}"))
                .unwrap_or_else(|| panic!("第 {index} 行未挂载"))
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

        fn row_center(&self, index: usize) -> LogicalPoint {
            self.center(self.row(index))
        }

        fn text_of(&self, node: NodeId) -> Option<SmolStr> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![node];
            while let Some(current) = stack.pop() {
                if let Ok(target) = tree.get(current) {
                    if let NodeKind::Text(content) = target.kind() {
                        return Some(content.text.clone());
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            None
        }

        fn trigger_text(&self, select: &Select) -> SmolStr {
            self.text_of(select.trigger_node().expect("触发器"))
                .expect("触发器里没有文本节点")
        }

        fn display_of(&self, node: NodeId) -> taffy::style::Display {
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .get(node)
                .expect("节点")
                .style()
                .inner()
                .display
        }

        fn idles(&mut self) -> bool {
            !self.app.drain().expect("帧").rendered
        }

        fn popup_entry(&self, select: &Select) -> PopupSnapshotEntry {
            let node = select.popup_node().expect("浮层内容节点");
            self.app
                .app()
                .popup_stack(self.window)
                .expect("浮层栈")
                .entries
                .into_iter()
                .find(|entry| entry.node == node)
                .expect("面板在栈里")
        }

        fn popup_rect(&self, select: &Select) -> LogicalRect {
            self.popup_entry(select).hit_rect
        }

        fn trigger_screen_rect(&self, select: &Select) -> LogicalRect {
            let trigger = select.trigger_node().expect("触发器");
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .node_screen_rect(trigger)
                .expect("触发器屏幕矩形")
        }
    }

    fn items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("a", "Alpha"),
            SelectItem::new("b", "Beta"),
            SelectItem::new("c", "Gamma").disabled(true),
        ]
    }

    #[test]
    fn a_single_select_chooses_one_item_and_shows_it_on_the_trigger() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().placeholder("请选择"));
        harness.mount(&select, items());
        assert_eq!(harness.trigger_text(&select), "请选择", "空选中显示占位");

        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()));
        let at = harness.row_center(1);
        harness.click(&select, at);

        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")],
            "单选写入选中集"
        );
        assert_eq!(harness.changed.borrow().len(), 1, "选中变化回调一次");
        assert!(!select.is_open(harness.app.app()), "单选选中即关");
        assert_eq!(harness.trigger_text(&select), "Beta", "触发器显示选中项");

        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "点击触发器打开");
        harness.click(&select, harness.center(trigger));
        assert!(!select.is_open(harness.app.app()), "再点触发器关闭");
    }

    #[test]
    fn a_multi_select_toggles_items_and_keeps_the_panel_open() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().multiple(true).max_tags(2));
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));

        let first = harness.row_center(0);
        harness.click(&select, first);
        assert!(select.is_open(harness.app.app()), "多选切换后面板保持打开");
        let second = harness.row_center(1);
        harness.click(&select, second);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("a"), SmolStr::new_static("b")],
            "多选累计选中"
        );
        assert_eq!(harness.changed.borrow().len(), 2);

        let tag0 = harness.find("select:tag:0").expect("标签 0");
        let tag1 = harness.find("select:tag:1").expect("标签 1");
        assert_eq!(harness.display_of(tag0), taffy::style::Display::Flex);
        assert_eq!(harness.display_of(tag1), taffy::style::Display::Flex);
        assert_eq!(harness.text_of(tag0), Some(SmolStr::new_static("Alpha")));
        assert_eq!(harness.text_of(tag1), Some(SmolStr::new_static("Beta")));

        let first = harness.row_center(0);
        harness.click(&select, first);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")],
            "再点切换回未选中"
        );
        assert_eq!(harness.display_of(tag1), taffy::style::Display::None);
    }

    #[test]
    fn a_multi_select_shows_an_overflow_counter_beyond_the_tag_limit() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().multiple(true).max_tags(1));
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let first = harness.row_center(0);
        harness.click(&select, first);
        let second = harness.row_center(1);
        harness.click(&select, second);

        let texts: Vec<SmolStr> = {
            let tree = harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree();
            let mut stack = vec![tree.root()];
            let mut found = Vec::new();
            while let Some(node) = stack.pop() {
                if let Ok(target) = tree.get(node) {
                    if let NodeKind::Text(content) = target.kind() {
                        found.push(content.text.clone());
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            found
        };
        assert!(
            texts.iter().any(|text| text == "+1"),
            "超出标签上限显示 +N：{texts:?}"
        );
    }

    #[test]
    fn a_disabled_item_cannot_be_chosen() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let at = harness.row_center(2);
        harness.click(&select, at);
        assert!(select.chosen(harness.app.app()).is_empty(), "禁用项不可选");
    }

    #[test]
    fn clearing_the_selection_empties_it_and_reports_once() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().clearable(true));
        harness.mount(&select, items());
        select.set_chosen(harness.app.app_mut(), ["b"], false);
        harness.settle(&select);
        assert_eq!(select.chosen(harness.app.app()).len(), 1);

        assert!(select.clear(harness.app.app_mut()), "清空成功");
        harness.settle(&select);
        assert!(select.chosen(harness.app.app()).is_empty());
        assert_eq!(harness.changed.borrow().len(), 1, "清空回调一次");
    }

    #[test]
    fn arrow_down_opens_the_panel_and_enter_chooses_the_highlighted_item() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        harness.focus_trigger(&select);
        harness.key(&select, KeyCode::ArrowDown);
        assert!(select.is_open(harness.app.app()), "↓ 打开面板");
        assert_eq!(select.highlighted(harness.app.app()), Some(0));
        harness.key(&select, KeyCode::End);
        assert_eq!(
            select.highlighted(harness.app.app()),
            Some(1),
            "End 到末个可选项"
        );
        harness.key(&select, KeyCode::Enter);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")]
        );
    }

    #[test]
    fn an_a11y_click_chooses_the_option_it_targets() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));

        let row = harness.find("select:panel:list:row:1").expect("选项行节点");
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(row).expect("行 a11y 记录");
        assert_eq!(record.role, A11yRole::ListBoxOption);
        assert!(
            record.actions.contains(&A11yAction::Click),
            "ListBoxOption 必须宣告 a11y Click：{:?}",
            record.actions
        );

        let action = Action::Custom(SmolStr::new_static("select:panel.row"));
        let handled = harness.app.app_mut().dispatch_action(
            harness.window,
            row,
            action,
            ActionSource::Programmatic,
        );
        assert!(handled, "AT Click 必须被分发到行 action");
        harness.settle(&select);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")],
            "AT Click 选中它指向的选项"
        );
        assert_eq!(harness.changed.borrow().len(), 1, "选中回调只触发一次");
        assert!(!select.is_open(harness.app.app()));
    }

    #[test]
    fn escape_and_outside_presses_close_the_panel_and_return_focus() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");

        harness.click(&select, harness.center(trigger));
        harness.key(&select, KeyCode::Escape);
        assert!(!select.is_open(harness.app.app()), "Esc 关闭");
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger),
            "关闭后焦点回到触发器"
        );

        harness.click(&select, harness.center(trigger));
        harness.click(&select, LogicalPoint::new(px(10.0), px(10.0)));
        assert!(!select.is_open(harness.app.app()), "点击外部关闭");
        assert_eq!(
            harness.background_clicks.get(),
            0,
            "点外部 (10,10) 没命中背景节点，只有浮层关闭"
        );
    }

    #[test]
    fn an_outside_press_replays_to_the_page() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        harness.click(&select, LogicalPoint::new(px(640.0), px(520.0)));
        assert!(!select.is_open(harness.app.app()), "点击外部关闭");
        assert_eq!(
            harness.background_clicks.get(),
            1,
            "D14：外部按下按 Replay 重派给下层命中控件"
        );
    }

    #[test]
    fn set_items_keeps_the_chosen_items_by_id() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        select.set_chosen(harness.app.app_mut(), ["b"], false);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")]
        );

        select
            .set_items(
                harness.app.app_mut(),
                vec![SelectItem::new("z", "Zeta"), SelectItem::new("b", "Beta 2")],
            )
            .expect("换条目");
        harness.settle(&select);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")],
            "按 id 保留选中"
        );
        assert_eq!(harness.trigger_text(&select), "Beta 2", "文本随条目更新");
    }

    #[test]
    fn the_panel_declares_list_and_list_box_option_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let panel = select.panel_node().expect("面板");
        let wrapper = select.popup_node().expect("浮层内容节点");
        assert_eq!(
            snapshot.node_for(wrapper).expect("包装").role,
            A11yRole::GenericContainer,
            "Select 面板不声明 Menu/ListBox 角色"
        );
        assert_eq!(
            snapshot
                .nodes()
                .iter()
                .filter(|node| node.role == A11yRole::List)
                .count(),
            1,
            "集合语义只来自内层虚拟列表容器（不得双 List）"
        );
        assert_eq!(
            snapshot.node_for(panel).expect("面板").role,
            A11yRole::GenericContainer
        );
        let options: Vec<String> = snapshot
            .nodes()
            .iter()
            .filter(|node| node.role == A11yRole::ListBoxOption)
            .filter_map(|node| node.label.as_ref().map(|label| label.text().to_owned()))
            .collect();
        assert!(
            options.contains(&"Alpha".to_owned()),
            "选项角色：{options:?}"
        );
    }

    #[test]
    fn the_read_points_report_the_items_the_placeholder_and_the_disabled_state() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().placeholder("请选择"));
        harness.mount(&select, items());

        let items = select.items();
        assert_eq!(items.len(), 3, "items() 是该选择器当前条目");
        assert_eq!(items[0].id, "a");
        assert_eq!(items[0].label, "Alpha");
        assert!(items[2].disabled, "条目自己的禁用态也读得回");
        assert_eq!(select.placeholder_text(), "请选择", "占位符读点");

        assert!(!select.is_disabled(harness.app.app()), "触发器默认可用");
        assert!(select.set_disabled(harness.app.app_mut(), true));
        assert!(
            select.is_disabled(harness.app.app()),
            "set_disabled 之后读回禁用"
        );
        assert!(select.set_disabled(harness.app.app_mut(), false));
        assert!(!select.is_disabled(harness.app.app()));
    }

    #[test]
    fn a_settled_select_with_no_input_requests_no_frames() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        assert!(!select.sync(harness.app.app_mut()).expect("同步"));
        assert!(harness.idles(), "稳态不出帧（I3）");
    }

    #[test]
    fn a_forwarded_pointer_up_does_not_toggle_the_row_twice() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions {
            multiple: true,
            ..SelectOptions::default()
        });
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let at = harness.row_center(0);
        harness.click(&select, at);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("a")],
            "一次点击只切换一次"
        );

        let forwarded = Event::Input(InputEvent::pointer_up(
            at,
            PointerButton::Primary,
            Modifiers::empty(),
        ));
        assert!(
            !select.dispatch(harness.app.app_mut(), &forwarded),
            "dispatch 不再兜底行点击"
        );
        harness.settle(&select);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("a")],
            "转发同一 PointerUp 后不重复切换"
        );
    }

    #[test]
    fn choosing_the_same_single_item_again_does_not_notify() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");

        harness.click(&select, harness.center(trigger));
        harness.click(&select, harness.row_center(1));
        assert_eq!(harness.changed.borrow().len(), 1, "首次选中回调一次");

        harness.click(&select, harness.center(trigger));
        harness.click(&select, harness.row_center(1));
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")]
        );
        assert_eq!(harness.changed.borrow().len(), 1, "重复选中同一项不再回调");
        assert!(!select.is_open(harness.app.app()), "单选选中即关");
    }

    #[test]
    fn the_panel_matches_the_trigger_width_for_short_items() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        let trigger_width = harness.bounds(trigger).size.width;
        harness.click(&select, harness.center(trigger));
        let panel = harness.bounds(select.panel_node().expect("面板"));
        assert!(
            (panel.size.width - trigger_width).abs() < px(0.5),
            "match width：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn a_long_item_grows_the_panel_past_its_trigger() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(
            &select,
            vec![
                SelectItem::new(
                    "long",
                    "A very long selection entry that cannot fit the trigger width at all",
                ),
                SelectItem::new("short", "B"),
            ],
        );
        let trigger = select.trigger_node().expect("触发器");
        let trigger_width = harness.bounds(trigger).size.width;
        harness.click(&select, harness.center(trigger));
        let panel = harness.bounds(select.panel_node().expect("面板"));
        assert!(
            panel.size.width > trigger_width + 100.0,
            "长条目把面板撑宽：面板 {} / 触发器 {trigger_width}",
            panel.size.width
        );
    }

    #[test]
    fn an_explicit_width_stays_exact_for_long_items() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default().width(px(120.0)));
        harness.mount(
            &select,
            vec![
                SelectItem::new(
                    "long",
                    "A very long selection entry that cannot fit the stated panel width at all",
                ),
                SelectItem::new("short", "B"),
            ],
        );
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let panel = harness.bounds(select.panel_node().expect("面板"));
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
    fn a_newline_in_a_label_does_not_distort_the_measured_width() {
        let mut single = Harness::new();
        let single_select = single.select(SelectOptions::default());
        single.mount(
            &single_select,
            vec![SelectItem::new("a", "MMMMMMMMMMMMMMMMMMMM")],
        );
        let trigger = single_select.trigger_node().expect("触发器");
        single.click(&single_select, single.center(trigger));
        let one_line = single
            .bounds(single_select.panel_node().expect("面板"))
            .size
            .width;

        let mut wrapped = Harness::new();
        let wrapped_select = wrapped.select(SelectOptions::default());
        wrapped.mount(
            &wrapped_select,
            vec![SelectItem::new("b", "MMMMMMMMMMMMMMMMMMMM\nMMMM")],
        );
        let trigger = wrapped_select.trigger_node().expect("触发器");
        wrapped.click(&wrapped_select, wrapped.center(trigger));
        let two_lines = wrapped
            .bounds(wrapped_select.panel_node().expect("面板"))
            .size
            .width;

        assert!(
            (one_line - two_lines).abs() < px(0.5),
            "换行 label 按最宽行量宽（与单行同宽）：单行 {one_line} / 换行 {two_lines}"
        );
    }

    #[test]
    fn clicking_a_row_keeps_enter_and_space_activating_it() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions {
            multiple: true,
            ..SelectOptions::default()
        });
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));

        harness.click(&select, harness.row_center(0));
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("a")],
            "点击切换一次"
        );
        let surface = harness.find("select:panel:surface").expect("面板 surface");
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(surface),
            "点击行后焦点回到面板键盘作用域"
        );

        harness.key(&select, KeyCode::Enter);
        assert!(
            select.chosen(harness.app.app()).is_empty(),
            "点击行后 Enter 仍切换高亮行"
        );
        harness.key(&select, KeyCode::Space);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("a")],
            "点击行后 Space 仍切换高亮行"
        );
    }

    #[test]
    fn tab_closes_the_select_and_moves_focus_past_the_trigger() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let after = harness.mount_focusable(
            "select-after",
            LogicalRect::new(px(0.0), px(450.0), px(80.0), px(32.0)),
        );
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "面板打开");

        harness.key(&select, KeyCode::Tab);
        assert!(
            !select.is_open(harness.app.app()),
            "Tab 必须离开并关闭非模态面板"
        );
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(after),
            "焦点移到触发器之后的下一个 tab stop"
        );
    }

    #[test]
    fn shift_tab_closes_the_select_and_moves_focus_before_the_trigger() {
        let mut harness = Harness::new();
        let before = harness.mount_focusable(
            "select-before",
            LogicalRect::new(px(0.0), px(100.0), px(80.0), px(32.0)),
        );
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "面板打开");

        harness.key_with(&select, KeyCode::Tab, Modifiers::SHIFT);
        assert!(
            !select.is_open(harness.app.app()),
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
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "面板打开");

        harness.key(&select, KeyCode::ArrowDown);
        assert_eq!(
            select.highlighted(harness.app.app()),
            Some(1),
            "方向键移动高亮到 Beta"
        );
        harness.key(&select, KeyCode::Enter);
        assert_eq!(
            select.chosen(harness.app.app()),
            vec![SmolStr::new_static("b")],
            "Enter 激活的是高亮行 Beta，而不是某个被 Tab 聚焦的行"
        );

        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "再次打开");
        let row = harness
            .find("select:panel:list:row:1")
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
        let select = harness.select(SelectOptions::default());
        harness.mount(
            &select,
            vec![
                SelectItem::new("alpha", "Alpha"),
                SelectItem::new("beta", "Beta"),
                SelectItem::new("charlie", "Charlie").disabled(true),
                SelectItem::new("cobalt", "Cobalt"),
                SelectItem::new("aurora", "Aurora"),
            ],
        );
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "面板打开");

        harness.key(&select, KeyCode::KeyA);
        assert_eq!(
            select.highlighted(harness.app.app()),
            Some(4),
            "A 从当前行循环到 Aurora"
        );
        harness.key(&select, KeyCode::KeyA);
        assert_eq!(
            select.highlighted(harness.app.app()),
            Some(0),
            "重复 A 循环回 Alpha"
        );
        harness.app.advance(Duration::from_millis(2100));
        harness.settle(&select);
        harness.key(&select, KeyCode::KeyC);
        assert_eq!(
            select.highlighted(harness.app.app()),
            Some(3),
            "C 跳过 disabled 的 Charlie 到 Cobalt"
        );
        harness.key(&select, KeyCode::KeyO);
        assert_eq!(
            select.highlighted(harness.app.app()),
            Some(3),
            "CO 前缀继续匹配 Cobalt"
        );
        assert!(
            select.chosen(harness.app.app()).is_empty(),
            "typeahead 只移动高亮，不选择"
        );
        assert!(select.is_open(harness.app.app()), "typeahead 不关闭面板");
    }

    #[test]
    fn row_hit_shells_are_not_in_the_tab_order() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()));

        let shell = harness
            .find("select:panel:list:row:0")
            .expect("行节点已挂载");
        let _ = harness.app.drain().expect("a11y 帧");
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(shell).expect("行 a11y");
        assert!(
            !record.actions.contains(&A11yAction::Focus),
            "行（含命中壳）不得进入 Tab 序：{:?}",
            record.actions
        );
        harness.key(&select, KeyCode::Tab);
        assert!(
            !select.is_open(harness.app.app()),
            "Tab 关闭面板而不是在行之间循环"
        );
    }

    #[test]
    fn item_icons_are_drawn_in_the_rows_and_on_the_trigger() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(
            &select,
            vec![
                SelectItem::new("a", "Alpha").icon(IconKind::Circle),
                SelectItem::new("b", "Beta"),
            ],
        );
        let trigger = select.trigger_node().expect("触发器");
        assert_eq!(
            harness.visible_images(trigger),
            1,
            "没有选中项时触发器只有 caret"
        );

        harness.click(&select, harness.center(trigger));
        let panel = select.panel_node().expect("面板");
        assert_eq!(
            harness.visible_images(panel),
            1,
            "只有带图标的条目画图标（其余行留占位槽）"
        );

        harness.click(&select, harness.row_center(0));
        harness.settle(&select);
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
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");

        harness.settle(&select);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("Select…"), "占位文本是可访问名");
        assert_eq!(
            record.has_popup,
            Some(HasPopup::ListBox),
            "关闭态静态 aria-haspopup=listbox"
        );
        assert_eq!(record.expanded, Some(false), "关闭态 aria-expanded=false");

        harness.click(&select, harness.center(trigger));
        harness.settle(&select);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(
            record.has_popup,
            Some(HasPopup::ListBox),
            "打开态 haspopup 仍保留"
        );
        assert_eq!(record.expanded, Some(true), "打开态 aria-expanded=true");

        harness.click(&select, harness.row_center(1));
        harness.settle(&select);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(record.label_text(), Some("Beta"), "可访问名跟随选中项");
        assert_eq!(
            record.has_popup,
            Some(HasPopup::ListBox),
            "关闭后静态 haspopup 不丢失"
        );
        assert_eq!(record.expanded, Some(false), "关闭后 expanded 复原");
    }

    #[test]
    fn a_multi_select_trigger_reports_a_selection_summary() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let select = harness.select(SelectOptions {
            multiple: true,
            ..SelectOptions::default()
        });
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        harness.click(&select, harness.row_center(0));
        harness.click(&select, harness.row_center(1));
        harness.settle(&select);
        let snapshot = a11y_snapshot_without_oracle(&mut harness.app);
        let record = snapshot.node_for(trigger).expect("触发器进 a11y");
        assert_eq!(
            record.label_text(),
            Some("Alpha +1"),
            "多选触发器给出选中摘要"
        );
    }

    #[test]
    fn a_select_keeps_the_tooltip_text_the_caller_supplied() {
        let mut harness = Harness::new();
        let select = harness
            .select(SelectOptions::default())
            .tooltip_text("Narrow the calendar by importance");
        assert_eq!(
            select.tooltip().as_deref(),
            Some("Narrow the calendar by importance"),
            "构建前后都可读"
        );
        harness.mount(&select, items());
        assert_eq!(
            select.tooltip().as_deref(),
            Some("Narrow the calendar by importance"),
            "mount 不消费 tooltip 文案"
        );

        let bare = harness.select(SelectOptions::default());
        assert_eq!(bare.tooltip(), None, "没有设置时是 None");
    }

    #[test]
    fn the_first_visible_frame_already_has_the_settled_rect() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        let trigger_width = harness.bounds(trigger).size.width;
        let _ = harness.app.drain().expect("帧");
        harness.press_release(harness.center(trigger));
        let _ = harness.app.drain().expect("帧");
        let first_entry = harness.popup_entry(&select);
        assert_eq!(
            first_entry.state,
            PopupState::Open,
            "首帧即 Open（不再需要等 reveal）"
        );
        let first = first_entry.hit_rect;
        let first_panel = harness.bounds(select.panel_node().expect("面板"));
        assert!(
            (first_panel.size.width - trigger_width).abs() < px(0.5),
            "首帧面板宽度已跟随触发器：面板 {} / 触发器 {trigger_width}",
            first_panel.size.width
        );
        harness.settle(&select);
        let stable = harness.popup_rect(&select);
        assert_eq!(first, stable, "首可见帧 rect 等于稳定帧");
        let stable_panel = harness.bounds(select.panel_node().expect("面板"));
        assert_eq!(
            first_panel.size.width, stable_panel.size.width,
            "首帧面板宽度等于稳定帧"
        );
    }

    #[test]
    fn the_select_slides_in_without_moving_the_settled_rect() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        let _ = harness.app.drain().expect("帧");
        harness.press_release(harness.center(trigger));
        let opening_frame = harness.app.drain().expect("打开帧").clone();
        let opening = harness.popup_entry(&select);
        assert_eq!(
            opening.paint_scale, 1.0,
            "FadeSlide 不做 scale（scale 只属于 FadeScale）"
        );
        assert!(
            opening.paint_offset.x == LogicalPx::ZERO
                && (opening.paint_offset.y - px(4.0)).abs() < px(0.5),
            "Select 必须与 Menu 一致使用 FadeSlide（打开帧向下 4px）：{:?}",
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
        harness.settle(&select);
        let settled = harness.popup_entry(&select);
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

    #[test]
    fn scrolling_the_trigger_out_of_the_page_closes_the_select() {
        let mut harness = Harness::new();
        let (page, scroll) = harness.scroll_page();
        let select = harness.select(SelectOptions::default());
        harness.mount_under(&select, items(), page);
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "面板打开");

        harness
            .app
            .app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(600.0)))
            .expect("滚动");
        harness.settle(&select);
        assert!(
            !select.is_open(harness.app.app()),
            "触发器滚出滚动页后关闭面板"
        );
    }

    #[test]
    fn losing_the_anchor_reports_anchor_lost() {
        let mut harness = Harness::new();
        let (page, scroll) = harness.scroll_page();
        let select = harness.select(SelectOptions::default());
        harness.mount_under(&select, items(), page);
        let reasons: Rc<RefCell<Vec<DismissReason>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = reasons.clone();
        select
            .inner
            .panel
            .on_close(move |_, reason| sink.borrow_mut().push(reason));
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "面板打开");

        harness
            .app
            .app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(600.0)))
            .expect("滚动");
        harness.settle(&select);
        assert!(!select.is_open(harness.app.app()), "锚点滚出视口后关闭面板");
        assert_eq!(
            reasons.borrow().as_slice(),
            &[DismissReason::AnchorLost],
            "锚点丢失/滚出视口关闭必须上报 AnchorLost（R2-02/Q2-07）"
        );
    }

    #[test]
    fn scrolling_a_visible_trigger_re_anchors_the_open_select() {
        let mut harness = Harness::new();
        let (page, scroll) = harness.scroll_page();
        harness.spacer(page, px(100.0));
        let select = harness.select(SelectOptions::default());
        harness.mount_under(&select, items(), page);
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()), "面板打开");
        let before = harness.popup_rect(&select);

        harness
            .app
            .app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(60.0)))
            .expect("滚动");
        harness.settle(&select);
        assert!(select.is_open(harness.app.app()), "触发器仍可见时保持打开");
        let after = harness.popup_rect(&select);
        assert_ne!(before, after, "滚动后浮层随锚点重新落位");
        let trigger_rect = harness.trigger_screen_rect(&select);
        assert!(
            (after.origin.y - (trigger_rect.origin.y + trigger_rect.size.height + px(4.0))).abs()
                < px(0.5),
            "浮层贴住触发器下沿：浮层 {after:?} / 触发器 {trigger_rect:?}"
        );
    }

    #[test]
    fn an_open_select_follows_a_window_resize() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        assert!(select.is_open(harness.app.app()));

        harness
            .app
            .app_mut()
            .window_mut(harness.window)
            .expect("窗口")
            .set_size(LogicalSize::new(px(320.0), px(240.0)));
        harness.settle(&select);

        let viewport = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .size();
        let panel = harness.popup_rect(&select);
        assert!(
            panel.max().x <= viewport.width + px(0.5)
                && panel.max().y <= viewport.height + px(0.5)
                && panel.origin.x >= px(3.5)
                && panel.origin.y >= px(3.5),
            "resize 后面板重新 settle 进视口（margin 4）：{panel:?} / viewport {viewport:?}"
        );
        let trigger_rect = harness.trigger_screen_rect(&select);
        assert!(
            (panel.origin.y - (trigger_rect.max().y + px(4.0)))
                .value()
                .abs()
                < 0.5
                || panel.max().y <= trigger_rect.origin.y + px(0.5),
            "面板仍锚定触发器（下方或翻到上方）：panel {panel:?} trigger {trigger_rect:?}"
        );
    }

    #[test]
    fn opening_and_closing_the_select_never_forces_a_full_repaint() {
        let mut harness = Harness::new();
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let id: PopupId = harness.popup_entry(&select).id;
        for _ in 0..4 {
            let record = harness.drain(&select);
            assert!(!record.damage_full, "打开面板不得全量重绘");
        }
        assert!(select.close(harness.app.app_mut()));
        for _ in 0..12 {
            harness.app.advance(Duration::from_millis(16));
            let record = harness.drain(&select);
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
        let select = harness.select(SelectOptions::default());
        harness.mount(&select, items());
        let trigger = select.trigger_node().expect("触发器");
        harness.click(&select, harness.center(trigger));
        let panel = select.panel_node().expect("面板");
        assert_eq!(harness.find("select:panel:panel"), Some(panel));
        assert!(harness.find("select:panel:surface").is_some());
        assert!(
            harness.find("select:panel:list:viewport").is_some(),
            "列表 viewport 命名保持"
        );
    }

    #[test]
    fn releasing_a_select_returns_its_signal_scope() {
        let mut app = TestApp::new();
        let window = app.window_id();
        let root = app.app().window(window).expect("窗口").tree().root();
        let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
        let baseline = signal_slot_stats(app.app());
        let mut steady_slots = None;
        for round in 0..4 {
            let select = Select::new(
                app.app_mut(),
                theme.clone(),
                format_smolstr!("select{round}"),
                SelectOptions::default(),
            );
            select.set_items(app.app_mut(), items()).expect("条目");
            let node = select.mount(app.app_mut(), window, root).expect("挂载");
            let _ = app.drain().expect("帧");
            let mounted = signal_slot_stats(app.app());
            assert!(
                mounted.live > baseline.live,
                "第 {round} 轮：挂载后占用信号槽（触发器 / 面板 / 自身三个信号）"
            );

            app.app_mut().remove_node(window, node).expect("卸载触发器");
            let released = select.release(app.app_mut());
            assert!(
                released >= 3,
                "第 {round} 轮：自身三个信号槽必须归还（实际 {released}）"
            );
            let after = signal_slot_stats(app.app());
            assert_eq!(
                after.live, baseline.live,
                "第 {round} 轮：release 后 live 回基线（缺口 = 3 ⇒ 这里停在基线 + 3）"
            );
            assert_eq!(select.release(app.app_mut()), 0, "重复释放幂等");

            match steady_slots {
                None => steady_slots = Some(after.slots),
                Some(slots) => assert_eq!(after.slots, slots, "第 {round} 轮：槽位复用，不增长"),
            }
        }
    }
}
