use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::{App, TimerId},
    error::EngineError,
    event::{Action, ActionSource, KeyBinding, KeyCode, Modifiers},
    interaction::ItemAlign,
    math::{LogicalPoint, LogicalPx, px},
    node::{
        builder::NodeBuilder, dependency::ReadCtx, id::NodeId, image::ImageFit, signal::Signal,
        tree::NodeTree,
    },
    paint::{style::StylePatch, style_builder::StyleBuilder},
    popup::{
        Alignment, Anchor, DismissOutcome, DismissPolicy, DismissReason, EscapePolicy, FocusPolicy,
        HasPopup, OutsidePress, PlacementSpec, PopupA11y, PopupAnimation, PopupDismiss, PopupError,
        PopupId, PopupSpec, PopupState, PopupTier, RestoreFocus, Side,
    },
    resources::ResourceId,
    text::{TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    kit::{
        icon::{ICON_INTRINSIC_SIZE, IconKind, svg},
        list::{List, ListOptions, RowHeight, Selection, SelectionMode},
        menu_chain::{HoverTimings, LevelId, LevelState, MenuChain, OpenSource},
        popup_panel::{
            AnchorChange, AnchorWatch, PanelMeasure as PopupMeasure, PanelWidth as PopupWidth,
            PopupPanel, PopupPanelOptions, PopupSurface,
        },
        size::UnifiedSize,
    },
    theme::{handle::ThemeHandle, role::Role},
};

const MENU_PADDING: LogicalPx = LogicalPx::new(4.0);
const GROUP_HEIGHT_RATIO: f32 = 0.75;
const SEPARATOR_HEIGHT: LogicalPx = LogicalPx::new(9.0);
const SUBMENU_GAP: LogicalPx = LogicalPx::ZERO;
const MENU_ROLES: PanelRoles = PanelRoles {
    panel: Some(A11yRole::Menu),
    row: A11yRole::MenuItem,
};

fn menu_dismiss(outside: OutsidePress) -> DismissPolicy {
    DismissPolicy::Chain {
        outside,
        escape: EscapePolicy::CloseTopmost,
        anchor_lost: true,
        focus_lost: true,
    }
}
const TYPEAHEAD_KEYS: [(KeyCode, char); 36] = [
    (KeyCode::KeyA, 'a'),
    (KeyCode::KeyB, 'b'),
    (KeyCode::KeyC, 'c'),
    (KeyCode::KeyD, 'd'),
    (KeyCode::KeyE, 'e'),
    (KeyCode::KeyF, 'f'),
    (KeyCode::KeyG, 'g'),
    (KeyCode::KeyH, 'h'),
    (KeyCode::KeyI, 'i'),
    (KeyCode::KeyJ, 'j'),
    (KeyCode::KeyK, 'k'),
    (KeyCode::KeyL, 'l'),
    (KeyCode::KeyM, 'm'),
    (KeyCode::KeyN, 'n'),
    (KeyCode::KeyO, 'o'),
    (KeyCode::KeyP, 'p'),
    (KeyCode::KeyQ, 'q'),
    (KeyCode::KeyR, 'r'),
    (KeyCode::KeyS, 's'),
    (KeyCode::KeyT, 't'),
    (KeyCode::KeyU, 'u'),
    (KeyCode::KeyV, 'v'),
    (KeyCode::KeyW, 'w'),
    (KeyCode::KeyX, 'x'),
    (KeyCode::KeyY, 'y'),
    (KeyCode::KeyZ, 'z'),
    (KeyCode::Digit0, '0'),
    (KeyCode::Digit1, '1'),
    (KeyCode::Digit2, '2'),
    (KeyCode::Digit3, '3'),
    (KeyCode::Digit4, '4'),
    (KeyCode::Digit5, '5'),
    (KeyCode::Digit6, '6'),
    (KeyCode::Digit7, '7'),
    (KeyCode::Digit8, '8'),
    (KeyCode::Digit9, '9'),
];

fn build_icon(
    tree: &mut NodeTree,
    parent: NodeId,
    resource: ResourceId,
    size: LogicalPx,
    bind_style: impl Fn(&mut ReadCtx<'_>) -> StylePatch + 'static,
) -> Result<(), EngineError> {
    NodeBuilder::image(resource, ImageFit::Fill)
        .style(StyleBuilder::new().size(size, size).build())
        .bind_style(bind_style)
        .a11y(A11yOverride {
            hidden: true,
            ..A11yOverride::default()
        })
        .build(tree, parent)
        .map(|_| ())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PanelRow {
    pub height: LogicalPx,
    pub disabled: bool,
}

pub(crate) struct PanelContent {
    pub rows: Vec<PanelRow>,
    pub build: Rc<dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>>,
    pub a11y: Rc<dyn Fn(usize) -> A11yPatch>,
    pub measure: Option<PanelMeasure>,
}

#[derive(Clone)]
pub(crate) struct PanelMeasure {
    pub labels: Vec<SmolStr>,
    pub extra: LogicalPx,
}

impl Clone for PanelContent {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            build: self.build.clone(),
            a11y: self.a11y.clone(),
            measure: self.measure.as_ref().map(|measure| PanelMeasure {
                labels: measure.labels.clone(),
                extra: measure.extra,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PanelRoles {
    pub panel: Option<A11yRole>,
    pub row: A11yRole,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ItemPanelOptions {
    pub size: UnifiedSize,
    pub max_height: LogicalPx,
    pub row_height: LogicalPx,
    pub placement: PlacementSpec,
    pub dismiss: DismissPolicy,
    pub tier: PopupTier,
    pub roles: PanelRoles,
    pub label: Option<SmolStr>,
    pub width: PanelWidth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelWidth {
    Exact,
    AtLeast,
}

pub(crate) struct PanelOpen {
    pub anchor: Anchor,
    pub opener: Option<NodeId>,
    pub focus: FocusPolicy,
    pub parent: Option<PopupId>,
    pub width: LogicalPx,
    pub a11y_parent: Option<NodeId>,
    pub surface_action: Option<Action>,
}

struct PanelActions {
    up: Action,
    down: Action,
    home: Action,
    end: Action,
    activate: Action,
    row: Action,
}

struct PanelState {
    window: Option<WindowId>,
    surface: Option<NodeId>,
    height: LogicalPx,
}

struct PanelInner {
    key: SmolStr,
    options: ItemPanelOptions,
    panel: PopupPanel,
    list: List,
    content: Rc<RefCell<Option<PanelContent>>>,
    actions: PanelActions,
    on_activate: RefCell<Option<Rc<dyn Fn(&mut App, usize)>>>,
    on_close: RefCell<Option<Rc<dyn Fn(&mut App, DismissReason)>>>,
    state: RefCell<PanelState>,
}

#[derive(Clone)]
pub(crate) struct ItemPanel {
    inner: Rc<PanelInner>,
}

impl ItemPanel {
    pub(crate) fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        options: ItemPanelOptions,
    ) -> Self {
        let key = key.into();
        let content: Rc<RefCell<Option<PanelContent>>> = Rc::new(RefCell::new(None));
        let list_options = ListOptions {
            row_height: RowHeight::Variable {
                estimate: options.row_height,
            },
            overscan: 4,
            selection: SelectionMode::Single,
            capacity: None,
            reverse: false,
            keep_on_newest: false,
            padding_x: MENU_PADDING,
            padding_y: LogicalPx::ZERO,
            row_stretch: true,
            scrollbar: true,
            separator: false,
        };
        let row_action = Action::Custom(format_smolstr!("{key}.row"));
        let list = {
            let shared = content.clone();
            let list_key = format_smolstr!("{key}:list");
            let row_action = row_action.clone();
            List::new(
                app,
                theme.clone(),
                list_key.clone(),
                list_options,
                move |tree, parent, index| {
                    let built = shared.borrow().as_ref().and_then(|content| {
                        content
                            .rows
                            .get(index)
                            .map(|row| (content.build.clone(), row.height))
                    });
                    let Some((build, height)) = built else {
                        return Ok(());
                    };
                    let hit = NodeBuilder::boxed()
                        .id(format_smolstr!("{list_key}:hit:{index}"))
                        .style(
                            StyleBuilder::new()
                                .flex_row()
                                .items_center()
                                .width_full()
                                .height(height)
                                .min_width_0()
                                .build(),
                        )
                        .tab_stop(false)
                        .on_click(row_action.clone())
                        .a11y(A11yOverride {
                            hidden: true,
                            ..A11yOverride::default()
                        })
                        .build(tree, parent)?;
                    build(tree, hit, index)
                },
            )
        };
        list.set_row_action(row_action.clone());
        let panel = PopupPanel::new(
            app,
            theme,
            key.clone(),
            PopupPanelOptions::new(PopupSurface::menu(), PopupWidth::Exact(px(1.0))),
        );
        let inner = Rc::new(PanelInner {
            actions: PanelActions {
                up: Action::Custom(format_smolstr!("{key}.up")),
                down: Action::Custom(format_smolstr!("{key}.down")),
                home: Action::Custom(format_smolstr!("{key}.home")),
                end: Action::Custom(format_smolstr!("{key}.end")),
                activate: Action::Custom(format_smolstr!("{key}.activate")),
                row: row_action,
            },
            key,
            options,
            panel: panel.clone(),
            list,
            content,
            on_activate: RefCell::new(None),
            on_close: RefCell::new(None),
            state: RefCell::new(PanelState {
                window: None,
                surface: None,
                height: LogicalPx::ZERO,
            }),
        });
        let weak = Rc::downgrade(&inner);
        panel.on_close(move |app, reason| {
            if let Some(inner) = weak.upgrade() {
                let panel = Self { inner };
                panel.reset_after_close(app);
                if let Some(callback) = panel.inner.on_close.borrow().clone() {
                    callback(app, reason);
                }
            }
        });
        let panel = Self { inner };
        panel.inner.list.on_activate({
            let panel = panel.clone();
            move |app, index| {
                panel.activate(app, index);
            }
        });
        panel.register_row_action(app);
        panel
    }

    pub(crate) fn panel_node(&self) -> Option<NodeId> {
        self.inner.panel.panel_node()
    }

    pub(crate) fn popup_node(&self) -> Option<NodeId> {
        self.inner.panel.popup_node()
    }

    pub(crate) fn id(&self) -> Option<PopupId> {
        self.inner.panel.id()
    }

    pub(crate) fn list(&self) -> &List {
        &self.inner.list
    }

    pub(crate) fn open_state(&self) -> Signal<bool> {
        self.inner.panel.open_state()
    }

    pub(crate) fn is_open(&self, app: &App) -> bool {
        self.inner.panel.is_open(app)
    }

    pub(crate) fn state(&self, app: &App) -> Option<PopupState> {
        self.inner.panel.state(app)
    }

    pub(crate) fn surface_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().surface
    }

    pub(crate) fn row_count(&self) -> usize {
        self.inner
            .content
            .borrow()
            .as_ref()
            .map_or(0, |content| content.rows.len())
    }

    pub(crate) fn row_disabled(&self, index: usize) -> bool {
        self.inner
            .content
            .borrow()
            .as_ref()
            .and_then(|content| content.rows.get(index))
            .is_none_or(|row| row.disabled)
    }

    pub(crate) fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.list.selection(app).first()
    }

    pub(crate) fn hovered(&self, app: &App) -> Option<usize> {
        app.read_with(&self.inner.list.hovered_signal(), |hovered| *hovered)
    }

    pub(crate) fn set_content(
        &self,
        app: &mut App,
        content: PanelContent,
    ) -> Result<(), EngineError> {
        let count = content.rows.len();
        let disabled: Vec<usize> = content
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.disabled.then_some(index))
            .collect();
        let a11y = content.a11y.clone();
        let measure = content.measure.clone();
        {
            let mut slot = self.inner.content.borrow_mut();
            *slot = Some(content);
        }
        self.inner.list.set_disabled_rows(disabled);
        self.inner.list.set_row_a11y(move |_cx, index| a11y(index));
        if self.inner.panel.is_open(app) {
            let previous = self
                .highlighted(app)
                .filter(|index| *index < count && !self.row_disabled(*index));
            self.inner.list.set_items(app, count)?;
            self.inner
                .panel
                .set_measure(app, self.popup_measure(measure.as_ref()));
            let height = self.inner.panel.available_body_height(app).map_or_else(
                || self.content_height(),
                |available| self.content_height().min(available),
            );
            self.write_height(app, height);
            self.inner.state.borrow_mut().height = height;
            let next = previous.or_else(|| (0..count).find(|index| !self.row_disabled(*index)));
            self.set_highlighted(app, next);
        }
        Ok(())
    }

    pub(crate) fn open(
        &self,
        app: &mut App,
        window: WindowId,
        request: PanelOpen,
    ) -> Result<bool, PopupError> {
        let content = self.inner.content.borrow().clone();
        let Some(content) = content else {
            return Ok(false);
        };
        self.set_width(app, request.width);
        self.inner
            .panel
            .set_measure(app, self.popup_measure(content.measure.as_ref()));
        if self.inner.panel.is_open(app) {
            if let Some(id) = self.inner.panel.id() {
                let _ = app.set_popup_anchor(id, request.anchor);
                let _ = app.raise_popup(id);
            }
            return Ok(false);
        }
        let height = self.content_height();
        let key = self.inner.key.clone();
        let list = self.inner.list.clone();
        let surface_action = request
            .surface_action
            .clone()
            .or_else(|| Some(self.inner.actions.activate.clone()));
        let bind_panel_keys = request.surface_action.is_none();
        let surface_slot: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let slot = surface_slot.clone();
        let mut spec = PopupSpec::new(request.anchor, self.inner.options.placement.clone());
        spec.parent = request.parent;
        spec.tier = self.inner.options.tier;
        spec.dismiss = self.inner.options.dismiss;
        spec.focus = request.focus;
        spec.opener = request.opener;
        spec.animation = PopupAnimation::FadeSlide;
        spec.a11y = PopupA11y {
            role: self.inner.options.roles.panel,
            label: self.inner.options.label.clone(),
            a11y_parent: request.a11y_parent,
            ..PopupA11y::default()
        };
        let opened = self
            .inner
            .panel
            .open(app, window, spec, move |app, window, body| {
                let surface = {
                    let Some(target) = app.window_mut(window) else {
                        return Err(EngineError::UnknownWindow { window });
                    };
                    let mut surface = NodeBuilder::boxed()
                        .id(format_smolstr!("{key}:surface"))
                        .style(
                            StyleBuilder::new()
                                .flex_col()
                                .flex_none()
                                .width_full()
                                .height(height)
                                .min_width_0()
                                .min_height_0()
                                .overflow_hidden()
                                .build(),
                        )
                        .tab_stop(true)
                        .a11y(A11yOverride {
                            role: Some(A11yRole::GenericContainer),
                            ..A11yOverride::default()
                        });
                    if let Some(action) = surface_action.clone() {
                        surface = surface.on_click(action);
                    }
                    surface.build(target.tree_mut(), body)?
                };
                slot.set(Some(surface));
                {
                    let Some(target) = app.window_mut(window) else {
                        return Err(EngineError::UnknownWindow { window });
                    };
                    list.build(target.tree_mut(), surface)?;
                }
                Ok(())
            })?;
        let Some(surface) = surface_slot.get() else {
            let _ = self.inner.panel.close_now(app);
            return Err(PopupError::UnknownPopup { id: opened });
        };
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = Some(window);
            state.surface = Some(surface);
            state.height = height;
        }
        let _ = app.refresh_text(window);
        if let Some(target) = app.window_mut(window) {
            let _ = target.compute_layout();
        }
        self.inner
            .list
            .attach(app, window)
            .map_err(|source| PopupError::ContentBuild {
                node: surface,
                source,
            })?;
        let wired = self
            .inner
            .list
            .set_items(app, content.rows.len())
            .map_err(|source| PopupError::ContentBuild {
                node: surface,
                source,
            });
        if let Err(error) = wired {
            let _ = self.inner.panel.close_now(app);
            return Err(error);
        }
        if bind_panel_keys {
            self.bind_panel_keys(app, window, surface)
                .map_err(|source| PopupError::ContentBuild {
                    node: surface,
                    source,
                })?;
        }
        Ok(true)
    }

    fn bind_panel_keys(
        &self,
        app: &mut App,
        window: WindowId,
        surface: NodeId,
    ) -> Result<(), EngineError> {
        for (key, action) in [
            (KeyCode::ArrowUp, self.inner.actions.up.clone()),
            (KeyCode::ArrowDown, self.inner.actions.down.clone()),
            (KeyCode::Home, self.inner.actions.home.clone()),
            (KeyCode::End, self.inner.actions.end.clone()),
        ] {
            app.bind_key_scoped(
                window,
                surface,
                KeyBinding {
                    key,
                    modifiers: Modifiers::empty(),
                    action,
                },
            )?;
        }
        Ok(())
    }

    pub(crate) fn set_width(&self, app: &mut App, width: LogicalPx) -> bool {
        let width = width.max(px(1.0));
        let mode = match self.inner.options.width {
            PanelWidth::Exact => PopupWidth::Exact(width),
            PanelWidth::AtLeast => PopupWidth::AtLeast { min: width },
        };
        self.inner.panel.set_width(app, mode)
    }

    fn content_height(&self) -> LogicalPx {
        let content = self.inner.content.borrow();
        let Some(content) = content.as_ref() else {
            return LogicalPx::ZERO;
        };
        let total = content
            .rows
            .iter()
            .fold(LogicalPx::ZERO, |total, row| total + row.height);
        total
            .min((self.inner.options.max_height - MENU_PADDING * 2.0).max(px(1.0)))
            .max(LogicalPx::ZERO)
    }

    fn popup_measure(&self, measure: Option<&PanelMeasure>) -> Option<PopupMeasure> {
        let measure = measure?;
        if measure.labels.is_empty() {
            return None;
        }
        Some(PopupMeasure {
            labels: measure.labels.clone(),
            extra: measure.extra + MENU_PADDING * 2.0,
            text_size: self.inner.options.size.text_size(),
        })
    }

    fn write_height(&self, app: &mut App, height: LogicalPx) {
        let (Some(window), Some(surface)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().surface,
        ) else {
            return;
        };
        if let Some(tree) = app.window_mut(window).map(|target| target.tree_mut())
            && let Ok(node) = tree.get(surface)
        {
            let mut style = node.style().clone();
            style.height_length(height);
            let _ = tree.set_style(surface, style);
        }
    }

    fn sync_height(&self, app: &mut App) -> bool {
        let Some(available) = self.inner.panel.available_body_height(app) else {
            return false;
        };
        let target = self.content_height().min(available);
        if (target - self.inner.state.borrow().height).value().abs() < 0.01 {
            return false;
        }
        self.write_height(app, target);
        self.inner.state.borrow_mut().height = target;
        true
    }

    fn reset_after_close(&self, app: &mut App) {
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = None;
            state.surface = None;
            state.height = LogicalPx::ZERO;
        }
        let _ = self.inner.list.clear_selection(app);
    }

    pub(crate) fn close(&self, app: &mut App) -> bool {
        self.inner.panel.close(app)
    }

    pub(crate) fn close_with_reason(&self, app: &mut App, reason: DismissReason) -> bool {
        self.inner.panel.close_with_reason(app, reason)
    }

    pub(crate) fn close_now(&self, app: &mut App) -> bool {
        self.inner.panel.close_now(app)
    }

    pub(crate) fn on_activate(&self, callback: impl Fn(&mut App, usize) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Rc::new(callback));
    }

    pub(crate) fn on_close(&self, callback: impl Fn(&mut App, DismissReason) + 'static) {
        *self.inner.on_close.borrow_mut() = Some(Rc::new(callback));
    }

    pub(crate) fn on_dismiss(
        &self,
        callback: impl Fn(&mut App, &PopupDismiss) -> DismissOutcome + 'static,
    ) {
        self.inner.panel.on_dismiss(callback);
    }

    pub(crate) fn set_a11y(&self, app: &mut App, a11y: PopupA11y) -> bool {
        self.inner.panel.set_a11y(app, a11y)
    }

    pub(crate) fn active_row_node(&self, app: &App) -> Option<NodeId> {
        self.highlighted(app)
            .and_then(|index| self.inner.list.row_node(app, index))
    }

    pub(crate) fn set_highlighted(&self, app: &mut App, index: Option<usize>) -> bool {
        let index = index.filter(|index| !self.row_disabled(*index));
        let mut selection = Selection::new();
        if let Some(index) = index {
            selection.select(index);
        }
        let changed = app.write(&self.inner.list.selection_signal(), selection);
        if changed && let Some(index) = index {
            let _ = self
                .inner
                .list
                .scroll_to_item(app, index, ItemAlign::Nearest);
        }
        changed
    }

    pub(crate) fn activate(&self, app: &mut App, index: usize) {
        if self.row_disabled(index) || index >= self.row_count() {
            return;
        }
        self.set_highlighted(app, Some(index));
        if let Some(callback) = self.inner.on_activate.borrow().clone() {
            callback(app, index);
        }
    }

    pub(crate) fn page_step(&self, app: &App) -> usize {
        let (Some(window), Some(surface)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().surface,
        ) else {
            return 1;
        };
        let height = app
            .window(window)
            .and_then(|target| target.node_screen_rect(surface))
            .map_or(LogicalPx::ZERO, |rect| rect.size.height);
        let usable = height - MENU_PADDING * 2.0;
        let rows = (usable / self.inner.options.row_height).floor();
        rows.max(1.0) as usize
    }

    pub(crate) fn focus(&self, app: &mut App) -> bool {
        let (Some(window), Some(surface)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().surface,
        ) else {
            return false;
        };
        app.set_focus(window, Some(surface))
    }

    pub(crate) fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.panel.sync(app);
        if !self.inner.panel.is_open(app) {
            return Ok(changed);
        }
        changed |= self.sync_height(app);
        changed |= self.inner.list.sync(app)?;
        Ok(changed)
    }

    pub(crate) fn release(&self, app: &mut App) -> usize {
        for action in [
            &self.inner.actions.up,
            &self.inner.actions.down,
            &self.inner.actions.home,
            &self.inner.actions.end,
            &self.inner.actions.activate,
            &self.inner.actions.row,
        ] {
            app.off_action(action);
        }
        self.inner.panel.release(app);
        self.inner.list.release(app)
    }

    fn row_at_node(&self, app: &App, window: WindowId, node: NodeId) -> Option<usize> {
        let rows = self.inner.list.mounted_rows(app);
        let tree = app.window(window)?.tree();
        let mut cursor = Some(node);
        while let Some(current) = cursor {
            if let Some((index, _)) = rows.iter().find(|(_, row)| *row == current) {
                return Some(*index);
            }
            cursor = tree.get(current).ok().and_then(|target| target.parent());
        }
        None
    }

    fn register_row_action(&self, app: &mut App) {
        for (action, step) in [
            (self.inner.actions.up.clone(), -1isize),
            (self.inner.actions.down.clone(), 1isize),
        ] {
            let this = self.clone();
            app.on_action(action, move |ctx| {
                this.move_highlight(ctx.app_mut(), step);
            });
        }
        for (action, last) in [
            (self.inner.actions.home.clone(), false),
            (self.inner.actions.end.clone(), true),
        ] {
            let this = self.clone();
            app.on_action(action, move |ctx| {
                this.jump_highlight(ctx.app_mut(), last);
            });
        }
        let this = self.clone();
        app.on_action(self.inner.actions.activate.clone(), move |ctx| {
            if matches!(ctx.source(), ActionSource::Click { .. }) && ctx.hit().is_some() {
                return;
            }
            if let Some(index) = this.highlighted(ctx.app_mut()) {
                this.activate(ctx.app_mut(), index);
            }
        });
        let this = self.clone();
        app.on_action(self.inner.actions.row.clone(), move |ctx| {
            let window = ctx.window();
            let node = ctx.hit().map_or_else(|| ctx.node(), |hit| hit.node);
            let app = ctx.app_mut();
            let Some(index) = this.row_at_node(app, window, node) else {
                return;
            };
            this.focus(app);
            this.activate(app, index);
        });
    }

    pub(crate) fn move_highlight(&self, app: &mut App, step: isize) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let current = self.highlighted(app);
        let next = match (current, step.is_positive()) {
            (Some(current), true) => (current + 1..count).find(|index| !self.row_disabled(*index)),
            (Some(current), false) => (0..current).rev().find(|index| !self.row_disabled(*index)),
            (None, true) => (0..count).find(|index| !self.row_disabled(*index)),
            (None, false) => (0..count).rev().find(|index| !self.row_disabled(*index)),
        };
        if next.is_some() {
            self.set_highlighted(app, next);
        }
    }

    pub(crate) fn jump_highlight(&self, app: &mut App, last: bool) {
        let count = self.row_count();
        let next = if last {
            (0..count).rev().find(|index| !self.row_disabled(*index))
        } else {
            (0..count).find(|index| !self.row_disabled(*index))
        };
        if next.is_some() {
            self.set_highlighted(app, next);
        }
    }
}

impl fmt::Debug for ItemPanel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ItemPanel")
            .field("key", &self.inner.key)
            .field("rows", &self.row_count())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuCheck {
    Checkbox,
    Radio,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MenuCheckState {
    pub kind: MenuCheck,
    pub checked: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuAction {
    pub id: SmolStr,
    pub label: SmolStr,
    pub icon: Option<IconKind>,
    pub shortcut: Option<SmolStr>,
    pub disabled: bool,
    pub danger: bool,
    pub check: Option<MenuCheckState>,
}

impl MenuAction {
    #[must_use]
    pub fn new(id: impl Into<SmolStr>, label: impl Into<SmolStr>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            icon: None,
            shortcut: None,
            disabled: false,
            danger: false,
            check: None,
        }
    }

    #[must_use]
    pub fn icon(mut self, icon: IconKind) -> Self {
        self.icon = Some(icon);
        self
    }

    #[must_use]
    pub fn shortcut(mut self, shortcut: impl Into<SmolStr>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }

    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    #[must_use]
    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    #[must_use]
    pub fn checkbox(mut self, checked: bool) -> Self {
        self.check = Some(MenuCheckState {
            kind: MenuCheck::Checkbox,
            checked,
        });
        self
    }

    #[must_use]
    pub fn radio(mut self, checked: bool) -> Self {
        self.check = Some(MenuCheckState {
            kind: MenuCheck::Radio,
            checked,
        });
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuSubmenu {
    pub label: SmolStr,
    pub icon: Option<IconKind>,
    pub disabled: bool,
    pub entries: Vec<MenuEntry>,
}

impl MenuSubmenu {
    #[must_use]
    pub fn new(label: impl Into<SmolStr>, entries: impl IntoIterator<Item = MenuEntry>) -> Self {
        Self {
            label: label.into(),
            icon: None,
            disabled: false,
            entries: entries.into_iter().collect(),
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntry {
    Action(MenuAction),
    Group(SmolStr),
    Separator,
    Submenu(MenuSubmenu),
}

#[derive(Clone, Debug, PartialEq)]
pub struct MenuOptions {
    pub size: UnifiedSize,
    pub width: LogicalPx,
    pub max_height: LogicalPx,
    pub placement: PlacementSpec,
    pub label: Option<SmolStr>,
    pub timings: HoverTimings,
    pub outside_press: OutsidePress,
}

impl Default for MenuOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            width: px(200.0),
            max_height: px(320.0),
            placement: PlacementSpec {
                side: Side::Bottom,
                align: Alignment::Start,
                gap: px(4.0),
                ..PlacementSpec::default()
            },
            label: None,
            timings: HoverTimings::default(),
            outside_press: OutsidePress::Replay,
        }
    }
}

impl MenuOptions {
    #[must_use]
    pub fn width(mut self, width: LogicalPx) -> Self {
        self.width = width;
        self
    }

    #[must_use]
    pub fn max_height(mut self, max_height: LogicalPx) -> Self {
        self.max_height = max_height;
        self
    }

    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn timings(mut self, timings: HoverTimings) -> Self {
        self.timings = timings;
        self
    }

    #[must_use]
    pub fn outside_press(mut self, outside_press: OutsidePress) -> Self {
        self.outside_press = outside_press;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuActivation {
    pub id: SmolStr,
    pub check: Option<MenuCheckState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    Action(usize),
    Submenu(usize),
    Group(usize),
    Separator,
}

struct MenuSink {
    on_activate: RefCell<Option<Rc<dyn Fn(&mut App, MenuActivation)>>>,
    on_close: RefCell<Option<Rc<dyn Fn(&mut App, DismissReason)>>>,
    chain: MenuChain,
    owners: RefCell<Vec<Option<Weak<MenuInner>>>>,
}

impl MenuSink {
    fn new(timings: HoverTimings) -> Self {
        Self {
            on_activate: RefCell::new(None),
            on_close: RefCell::new(None),
            chain: MenuChain::new(timings),
            owners: RefCell::new(Vec::new()),
        }
    }

    fn chain(&self) -> &MenuChain {
        &self.chain
    }

    fn register(&self, level: LevelId, owner: &Rc<MenuInner>) {
        let mut owners = self.owners.borrow_mut();
        if owners.len() <= level.index() {
            owners.resize(level.index() + 1, None);
        }
        owners[level.index()] = Some(Rc::downgrade(owner));
    }

    fn owner(&self, level: LevelId) -> Option<Menu> {
        self.owners
            .borrow()
            .get(level.index())
            .and_then(|owner| owner.as_ref())
            .and_then(Weak::upgrade)
            .map(|inner| Menu { inner })
    }
}

struct OpenSubmenu {
    row: usize,
    level: LevelId,
    menu: Menu,
}

#[derive(Clone)]
pub struct Menu {
    inner: Rc<MenuInner>,
}

struct MenuSeed {
    theme: ThemeHandle,
    options: MenuOptions,
    kind: MenuKind,
    parent: Option<(Weak<MenuInner>, usize)>,
    a11y_parent: Option<NodeId>,
    sink: Rc<MenuSink>,
    level: Option<LevelId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuKind {
    Root,
    Submenu,
}

struct MenuInner {
    key: SmolStr,
    theme: ThemeHandle,
    options: MenuOptions,
    panel: ItemPanel,
    icons: RefCell<IconSnapshot>,
    rows: RefCell<Vec<RowKind>>,
    entries: RefCell<Rc<Vec<MenuEntry>>>,
    kind: MenuKind,
    parent: Option<(Weak<MenuInner>, usize)>,
    a11y_parent: Option<NodeId>,
    sink: Rc<MenuSink>,
    submenus: RefCell<Vec<OpenSubmenu>>,
    level: Cell<Option<LevelId>>,
    active_descendant: Cell<Option<NodeId>>,
    pending_open: Cell<Option<LevelId>>,
    pending_timer: Cell<Option<TimerId>>,
    grace_timer: Cell<Option<TimerId>>,
    close_timer: Cell<Option<TimerId>>,
    closing_root: Cell<Option<LevelId>>,
    typeahead_timer: Cell<Option<TimerId>>,
    trigger_action: RefCell<Option<Action>>,
    opener: Cell<Option<NodeId>>,
    trigger_over: Cell<bool>,
    last_hovered: Cell<Option<usize>>,
    window: Cell<Option<WindowId>>,
    watch: RefCell<AnchorWatch>,
    watch_animation: Cell<bool>,
    scroll_closes: Cell<bool>,
    internal_scroll: Cell<Option<LogicalPx>>,
    right_action: Action,
    left_action: Action,
    up_action: Action,
    down_action: Action,
    home_action: Action,
    end_action: Action,
    surface_action: Action,
    tab_next_action: Action,
    tab_prev_action: Action,
    page_up_action: Action,
    page_down_action: Action,
    typeahead_actions: RefCell<Vec<(char, Action)>>,
}

impl Menu {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        entries: impl IntoIterator<Item = MenuEntry>,
        options: MenuOptions,
    ) -> Self {
        let timings = options.timings;
        Self::build(
            app,
            key,
            entries,
            MenuSeed {
                theme,
                options,
                kind: MenuKind::Root,
                parent: None,
                a11y_parent: None,
                sink: Rc::new(MenuSink::new(timings)),
                level: None,
            },
        )
    }

    fn build(
        app: &mut App,
        key: impl Into<SmolStr>,
        entries: impl IntoIterator<Item = MenuEntry>,
        seed: MenuSeed,
    ) -> Self {
        let MenuSeed {
            theme,
            options,
            kind,
            parent,
            a11y_parent,
            sink,
            level,
        } = seed;
        let key = key.into();
        let panel = ItemPanel::new(
            app,
            theme.clone(),
            format_smolstr!("{key}:panel"),
            ItemPanelOptions {
                size: options.size,
                max_height: options.max_height,
                row_height: options.size.h(),
                placement: options.placement.clone(),
                dismiss: menu_dismiss(options.outside_press),
                tier: PopupTier::Auto,
                roles: MENU_ROLES,
                label: options.label.clone(),
                width: PanelWidth::Exact,
            },
        );
        let inner = Rc::new(MenuInner {
            key: key.clone(),
            theme,
            options,
            panel,
            icons: RefCell::new(IconSnapshot::default()),
            rows: RefCell::new(Vec::new()),
            entries: RefCell::new(Rc::new(entries.into_iter().collect())),
            kind,
            parent,
            a11y_parent,
            sink,
            submenus: RefCell::new(Vec::new()),
            level: Cell::new(level),
            active_descendant: Cell::new(None),
            pending_open: Cell::new(None),
            pending_timer: Cell::new(None),
            grace_timer: Cell::new(None),
            close_timer: Cell::new(None),
            closing_root: Cell::new(None),
            typeahead_timer: Cell::new(None),
            trigger_action: RefCell::new(None),
            opener: Cell::new(None),
            trigger_over: Cell::new(false),
            last_hovered: Cell::new(None),
            window: Cell::new(None),
            watch: RefCell::new(AnchorWatch::default()),
            watch_animation: Cell::new(false),
            scroll_closes: Cell::new(false),
            internal_scroll: Cell::new(None),
            right_action: Action::Custom(format_smolstr!("{key}.right")),
            left_action: Action::Custom(format_smolstr!("{key}.left")),
            up_action: Action::Custom(format_smolstr!("{key}.up")),
            down_action: Action::Custom(format_smolstr!("{key}.down")),
            home_action: Action::Custom(format_smolstr!("{key}.home")),
            end_action: Action::Custom(format_smolstr!("{key}.end")),
            surface_action: Action::Custom(format_smolstr!("{key}.surface")),
            tab_next_action: Action::Custom(format_smolstr!("{key}.tab.next")),
            tab_prev_action: Action::Custom(format_smolstr!("{key}.tab.prev")),
            page_up_action: Action::Custom(format_smolstr!("{key}.page.up")),
            page_down_action: Action::Custom(format_smolstr!("{key}.page.down")),
            typeahead_actions: RefCell::new(
                TYPEAHEAD_KEYS
                    .iter()
                    .map(|(_, character)| {
                        (
                            *character,
                            Action::Custom(format_smolstr!("{key}.type.{character}")),
                        )
                    })
                    .collect(),
            ),
        });
        let menu = Self { inner };
        menu.install_content(app);
        menu.install_handlers(app);
        menu
    }

    fn chain(&self) -> &MenuChain {
        self.inner.sink.chain()
    }

    fn level(&self) -> Option<LevelId> {
        self.inner.level.get()
    }

    fn register_level(&self, level: LevelId) {
        self.inner.level.set(Some(level));
        self.inner.sink.register(level, &self.inner);
    }

    pub fn set_entries(
        &self,
        app: &mut App,
        entries: impl IntoIterator<Item = MenuEntry>,
    ) -> Result<(), EngineError> {
        *self.inner.entries.borrow_mut() = Rc::new(entries.into_iter().collect());
        self.close_submenus(app);
        self.install_content(app);
        Ok(())
    }

    #[must_use]
    pub fn entries(&self) -> Rc<Vec<MenuEntry>> {
        self.inner.entries.borrow().clone()
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
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.panel.panel_node()
    }

    #[must_use]
    pub fn popup_node(&self) -> Option<NodeId> {
        self.inner.panel.popup_node()
    }

    #[must_use]
    pub fn row_count(&self) -> usize {
        self.inner.rows.borrow().len()
    }

    fn row_disabled(&self, index: usize) -> bool {
        self.inner.panel.row_disabled(index)
    }

    #[must_use]
    pub fn highlighted(&self, app: &App) -> Option<usize> {
        self.inner.panel.highlighted(app)
    }

    pub fn open(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
    ) -> Result<bool, PopupError> {
        self.open_focused(app, window, anchor, true, OpenSource::Click)
    }

    pub fn open_submenu(&self, app: &mut App, row: usize) -> Result<Option<Menu>, PopupError> {
        let Some(index) = self.entry_index(row) else {
            return Ok(None);
        };
        self.open_submenu_row(app, row, index, OpenSource::Keyboard, None)
    }

    fn open_focused(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        take_focus: bool,
        source: OpenSource,
    ) -> Result<bool, PopupError> {
        self.inner.window.set(Some(window));
        self.close_submenus(app);
        self.resolve_icons(app, window)
            .map_err(|source| popup_error(app, window, source))?;
        self.install_content(app);
        let parent = match self.inner.kind {
            MenuKind::Root => None,
            MenuKind::Submenu => self.parent_popup_id(),
        };
        let focus = if self.inner.kind == MenuKind::Root || take_focus {
            FocusPolicy::Take {
                restore: RestoreFocus::Previous,
            }
        } else {
            FocusPolicy::None
        };
        let opener = match anchor {
            Anchor::Node { node } => Some(node),
            Anchor::NodeRect { node, .. } => Some(node),
            Anchor::WindowRect(_) => None,
        };
        self.inner.opener.set(opener);
        self.inner.watch.borrow_mut().reset();
        self.inner.watch_animation.set(false);
        self.inner.internal_scroll.set(None);
        let level = match self.level() {
            Some(level) => level,
            None => {
                let level = self.chain().open_root(None, source);
                self.inner.level.set(Some(level));
                level
            }
        };
        self.register_level(level);
        let opened = match self.inner.panel.open(
            app,
            window,
            PanelOpen {
                anchor,
                opener,
                focus,
                parent,
                width: self.inner.options.width,
                a11y_parent: self.inner.a11y_parent,
                surface_action: Some(self.inner.surface_action.clone()),
            },
        ) {
            Ok(opened) => opened,
            Err(error) => {
                if self.inner.kind == MenuKind::Root {
                    self.chain().reset();
                    self.inner.level.set(None);
                } else if let Some(level) = self.level() {
                    self.chain().cancel_open(level);
                }
                self.inner.active_descendant.set(None);
                return Err(error);
            }
        };
        if opened && let Err(source) = self.bind_extra_keys(app, window) {
            let error = popup_error(app, window, source);
            self.close_now(app);
            return Err(error);
        }
        if opened {
            let rect = self.popup_rect(app);
            self.chain().confirm_open(level, rect, source);
            if let Some(pointer) = app.popup_pointer(window) {
                self.chain().set_exit(level, pointer);
            }
            self.chain().compute_grace(level);
            let first = (0..self.row_count()).find(|row| !self.row_disabled(*row));
            self.set_active_row(app, first);
        }
        Ok(opened)
    }

    pub fn open_at(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        highlighted: Option<&str>,
    ) -> Result<bool, PopupError> {
        let opened = self.open(app, window, anchor)?;
        if let Some(id) = highlighted
            && let Some(row) = self.row_of(id)
        {
            self.inner.panel.set_highlighted(app, Some(row));
            self.sync_active_row(app);
        }
        Ok(opened)
    }

    fn popup_rect(&self, app: &App) -> Option<ui_core::math::LogicalRect> {
        let id = self.inner.panel.id()?;
        app.popup_rect(id).map(|rect| rect.hit)
    }

    fn set_active_row(&self, app: &mut App, row: Option<usize>) -> bool {
        let Some(level) = self.level() else {
            return false;
        };
        let changed = self.inner.panel.set_highlighted(app, row);
        self.chain().set_active(level, row);
        let _ = self.sync_active_descendant(app);
        changed
    }

    fn sync_active_descendant(&self, app: &mut App) -> bool {
        let active = self.inner.panel.active_row_node(app);
        if self.inner.active_descendant.get() == active {
            return false;
        }
        self.inner.active_descendant.set(active);
        let a11y = PopupA11y {
            role: MENU_ROLES.panel,
            label: self.inner.options.label.clone(),
            a11y_parent: self.inner.a11y_parent,
            active_descendant: active,
            ..PopupA11y::default()
        };
        self.inner.panel.set_a11y(app, a11y)
    }

    pub fn close(&self, app: &mut App) -> bool {
        self.close_submenus(app);
        self.inner.panel.close(app)
    }

    pub fn close_now(&self, app: &mut App) -> bool {
        self.close_submenus(app);
        self.inner.panel.close_now(app)
    }

    pub fn on_activate(self, callback: impl Fn(&mut App, MenuActivation) + 'static) -> Self {
        if self.inner.kind == MenuKind::Root {
            *self.inner.sink.on_activate.borrow_mut() = Some(Rc::new(callback));
        }
        self
    }

    pub fn on_close(self, callback: impl Fn(&mut App, DismissReason) + 'static) -> Self {
        if self.inner.kind == MenuKind::Root {
            *self.inner.sink.on_close.borrow_mut() = Some(Rc::new(callback));
        }
        self
    }

    pub fn bind_trigger(
        &self,
        app: &mut App,
        window: WindowId,
        trigger: NodeId,
    ) -> Result<(), EngineError> {
        if let Some(previous) = self.inner.trigger_action.borrow_mut().take() {
            app.off_action(&previous);
        }
        let action = Action::Custom(format_smolstr!("{}.trigger", self.inner.key));
        let this = self.clone();
        app.on_action(action.clone(), move |ctx| {
            let anchor = Anchor::Node { node: ctx.node() };
            let window = ctx.window();
            let app = ctx.app_mut();
            let _ = this.open(app, window, anchor);
        });
        self.inner.trigger_action.replace(Some(action.clone()));
        app.bind_key_scoped(
            window,
            trigger,
            KeyBinding {
                key: KeyCode::F10,
                modifiers: Modifiers::ALT,
                action,
            },
        )
    }

    pub(crate) fn closing_on_scroll(self) -> Self {
        self.inner.scroll_closes.set(true);
        self
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.sync_tree(app)?;
        if self.inner.kind == MenuKind::Root && self.inner.panel.is_open(app) {
            changed |= self.sync_chain(app)?;
        }
        Ok(changed)
    }

    fn sync_tree(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.sync_own(app)?;
        if !self.inner.panel.is_open(app) {
            return Ok(changed);
        }
        let submenus: Vec<Menu> = self
            .inner
            .submenus
            .borrow()
            .iter()
            .map(|open| open.menu.clone())
            .collect();
        for submenu in &submenus {
            changed |= submenu.sync_tree(app)?;
        }
        self.retain_open_submenus(app);
        Ok(changed)
    }

    fn sync_own(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.panel.sync(app)?;

        match self.inner.panel.state(app) {
            None | Some(PopupState::Closed) => {
                self.on_level_closed(app);
                return Ok(changed);
            }
            Some(PopupState::Opening | PopupState::Open) => {
                changed |= self.reconcile_chain(app);
            }
            Some(PopupState::DismissPending | PopupState::Closing) => {
                self.cancel_pending_opens(app);
                if let Some(level) = self.level() {
                    self.chain().mark_closing(level);
                }
                return Ok(changed);
            }
        }
        if self.root().inner.panel.state(app) != Some(PopupState::Opening) && self.sync_anchor(app)
        {
            changed = true;
            return Ok(changed);
        }
        changed |= self.sync_active_row(app);
        Ok(changed)
    }

    fn reconcile_chain(&self, app: &mut App) -> bool {
        let Some(level) = self.level() else {
            return false;
        };
        match self.chain().state(level) {
            Some(LevelState::Closing) => {
                let root = self.root();
                if root.inner.close_timer.get().is_some() || root.inner.closing_root.get().is_some()
                {
                    return false;
                }
                self.inner.last_hovered.set(None);
                self.inner.trigger_over.set(false);
                self.inner.active_descendant.set(None);
                self.chain().reopen(level)
            }
            Some(LevelState::Closed) => {
                let rect = self.popup_rect(app);
                self.inner.last_hovered.set(None);
                self.inner.trigger_over.set(false);
                self.inner.active_descendant.set(None);
                if self.inner.kind == MenuKind::Root {
                    let level = self.chain().open_root(rect, OpenSource::Click);
                    self.inner.level.set(Some(level));
                    self.register_level(level);
                    self.inner.active_descendant.set(None);
                    return true;
                }
                if self.chain().revive(level, rect) {
                    self.inner.active_descendant.set(None);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn on_level_closed(&self, app: &mut App) {
        if self.level().is_some() {
            self.close_submenus(app);
            if let Some(level) = self.level() {
                self.chain().close(level);
            }
        }
        self.inner.last_hovered.set(None);
        self.inner.trigger_over.set(false);
        self.inner.active_descendant.set(None);

        if self.inner.kind == MenuKind::Root {
            self.chain().reset();
            self.inner.level.set(None);
            self.cancel_timers(app);
        }
    }

    pub fn release(&self, app: &mut App) -> usize {
        self.close_submenus(app);
        self.cancel_timers(app);
        app.off_action(&self.inner.right_action);
        app.off_action(&self.inner.left_action);
        app.off_action(&self.inner.up_action);
        app.off_action(&self.inner.down_action);
        app.off_action(&self.inner.home_action);
        app.off_action(&self.inner.end_action);
        app.off_action(&self.inner.surface_action);
        app.off_action(&self.inner.tab_next_action);
        app.off_action(&self.inner.tab_prev_action);
        app.off_action(&self.inner.page_up_action);
        app.off_action(&self.inner.page_down_action);
        for (_, action) in self.inner.typeahead_actions.borrow().iter() {
            app.off_action(action);
        }
        if let Some(action) = self.inner.trigger_action.borrow_mut().take() {
            app.off_action(&action);
        }
        let mut released = self.inner.panel.release(app);
        let submenus: Vec<Menu> = self
            .inner
            .submenus
            .borrow_mut()
            .drain(..)
            .map(|open| open.menu)
            .collect();
        for submenu in submenus {
            released += submenu.release(app);
        }
        released
    }

    fn cancel_timers(&self, app: &mut App) {
        for timer in [
            self.inner.pending_timer.take(),
            self.inner.grace_timer.take(),
            self.inner.close_timer.take(),
            self.inner.typeahead_timer.take(),
        ]
        .into_iter()
        .flatten()
        {
            app.cancel_timer(&timer);
        }
        self.inner.pending_open.set(None);
        self.inner.closing_root.set(None);
    }

    fn parent_popup_id(&self) -> Option<PopupId> {
        let (parent, _) = self.inner.parent.as_ref()?;
        let parent = Menu {
            inner: parent.upgrade()?,
        };
        parent.inner.panel.id()
    }

    fn chain_pointer(&self, app: &App) -> Option<LogicalPoint> {
        let window = self.inner.window.get()?;
        app.hovered(window)?;
        app.popup_pointer(window)
    }

    fn dismiss_outcome(&self, app: &App, dismiss: &PopupDismiss) -> DismissOutcome {
        if dismiss.reason != DismissReason::OutsidePress {
            return DismissOutcome::Close;
        }
        let Some(pointer) = dismiss.pointer.or_else(|| self.chain_pointer(app)) else {
            return DismissOutcome::Close;
        };
        if self.chain().safe_area_contains(pointer) {
            DismissOutcome::Ignore
        } else {
            DismissOutcome::Close
        }
    }

    fn sync_chain(&self, app: &mut App) -> Result<bool, EngineError> {
        let Some(window) = self.inner.window.get() else {
            return Ok(false);
        };
        let mut changed = self.refresh_geometry(app)?;
        let Some(pointer) = self.chain_pointer(app) else {
            changed |= self.pointer_left(app, window)?;
            return Ok(changed);
        };
        let chain = self.chain();
        if !chain.contains(pointer) {
            changed |= self.pointer_left(app, window)?;
            return Ok(changed);
        }
        chain.note_pointer_entered();
        changed |= self.cancel_pending_close(app);
        let hit_level = chain.open_levels().into_iter().rev().find(|level| {
            chain
                .rect(*level)
                .is_some_and(|rect| rect.contains(pointer))
        });
        match hit_level {
            Some(level) => {
                if let Some(menu) = self.inner.sink.owner(level) {
                    changed |= menu.update_hover(app, pointer)?;
                }
            }
            None => {
                if chain.grace_contains(pointer).is_some() {
                    return Ok(changed);
                }
                changed |= self.pointer_left(app, window)?;
            }
        }
        Ok(changed)
    }

    fn refresh_geometry(&self, app: &mut App) -> Result<bool, EngineError> {
        let chain = self.chain();
        let mut changed = false;
        for level in chain.open_levels() {
            let Some(menu) = self.inner.sink.owner(level) else {
                continue;
            };
            if let Some(rect) = menu.popup_rect(app)
                && chain.set_rect(level, rect)
            {
                changed = true;
                if chain.has_grace(level) {
                    changed |= chain.compute_grace(level);
                    self.rearm_grace_timer(app, level)?;
                }
            }
            let (Some(row), Some(parent)) = (chain.row(level), chain.parent(level)) else {
                continue;
            };
            let Some(parent_menu) = self.inner.sink.owner(parent) else {
                continue;
            };
            if let Some(anchor) = parent_menu.row_rect(app, row)
                && chain.set_anchor(level, anchor)
            {
                changed = true;
                if chain.has_grace(level) {
                    changed |= chain.compute_grace(level);
                    self.rearm_grace_timer(app, level)?;
                }
            }
        }
        Ok(changed)
    }

    fn rearm_grace_timer(&self, app: &mut App, level: LevelId) -> Result<(), EngineError> {
        let Some(parent) = self.chain().parent(level) else {
            return Ok(());
        };
        let Some(parent_menu) = self.inner.sink.owner(parent) else {
            return Ok(());
        };
        parent_menu.arm_grace_timer(app, level)
    }

    fn row_rect(&self, app: &App, row: usize) -> Option<ui_core::math::LogicalRect> {
        let window = self.inner.window.get()?;
        let node = self.inner.panel.list().row_node(app, row)?;
        app.window(window)?.node_screen_rect(node)
    }

    fn row_contains(&self, app: &App, row: usize, point: LogicalPoint) -> bool {
        self.row_rect(app, row)
            .is_some_and(|rect| rect.contains(point))
    }

    fn update_hover(&self, app: &mut App, pointer: LogicalPoint) -> Result<bool, EngineError> {
        let Some(level) = self.level() else {
            return Ok(false);
        };
        let chain = self.chain();
        let mut changed = false;
        if let Some(child) = chain.child(level) {
            let trigger_row = chain.row(child).unwrap_or(0);
            let over_trigger = self.row_contains(app, trigger_row, pointer);
            let was_over = self.inner.trigger_over.get();
            self.inner.trigger_over.set(over_trigger);
            if over_trigger {
                changed |= chain.set_exit(child, pointer);
                changed |= self.clear_grace(app, child);
                changed |= self.cancel_pending_close(app);
            } else {
                if !chain.has_grace(child) && was_over {
                    changed |= chain.compute_grace(child);
                    self.arm_grace_timer(app, child)?;
                }
                if chain.grace_contains(pointer).is_some() {
                    changed |= self.cancel_pending_close(app);
                    return Ok(changed);
                }
            }
        }
        let hovered = self.inner.panel.hovered(app);
        if hovered == self.inner.last_hovered.get() {
            return Ok(changed);
        }
        self.inner.last_hovered.set(hovered);
        let Some(row) = hovered else {
            return Ok(changed);
        };
        let _ = self.cancel_pending_open(app);
        match self.row_kind(row) {
            Some(RowKind::Submenu(index)) => {
                let _ = self.close_submenus(app);
                let _ = self.set_active_row(app, Some(row));
                let _ = self.start_pending_open(app, row, index)?;
            }
            Some(RowKind::Action(_) | RowKind::Group(_) | RowKind::Separator) => {
                let _ = self.close_submenus(app);
                let _ = self.set_active_row(app, Some(row));
            }
            None => {}
        }
        Ok(true)
    }

    fn clear_grace(&self, app: &mut App, child: LevelId) -> bool {
        if let Some(timer) = self.inner.grace_timer.take() {
            app.cancel_timer(&timer);
        }
        self.chain().clear_grace(child)
    }

    fn arm_grace_timer(&self, app: &mut App, child: LevelId) -> Result<(), EngineError> {
        if let Some(timer) = self.inner.grace_timer.take() {
            app.cancel_timer(&timer);
        }
        let Some(window) = self.inner.window.get() else {
            return Ok(());
        };
        let delay = self.chain().timings().close_grace;
        let chain = self.chain().clone();

        let timer = app.after(window, delay, move |app| {
            if chain.clear_grace(child) {
                let _ = app.request_repaint(window);
            }
        })?;
        self.inner.grace_timer.set(Some(timer));
        Ok(())
    }

    fn start_pending_open(
        &self,
        app: &mut App,
        row: usize,
        index: usize,
    ) -> Result<bool, EngineError> {
        let Some(level) = self.level() else {
            return Ok(false);
        };
        let Some(window) = self.inner.window.get() else {
            return Ok(false);
        };
        if !self
            .inner
            .panel
            .state(app)
            .is_some_and(PopupState::is_dismissable)
            || self.chain().state(level) != Some(LevelState::Open)
        {
            return Ok(false);
        }
        let delay = self.chain().timings().open_delay;
        let child = self.chain().begin_open(level, row, OpenSource::Pointer);
        self.inner.pending_open.set(Some(child));
        let this = self.clone();
        let timer = app.after(window, delay, move |app| {
            this.fire_pending_open(app, child, row, index);
        })?;
        self.inner.pending_timer.set(Some(timer));
        Ok(true)
    }

    fn fire_pending_open(&self, app: &mut App, child: LevelId, row: usize, index: usize) {
        self.inner.pending_timer.set(None);
        if self.inner.pending_open.get() != Some(child) {
            return;
        }
        self.inner.pending_open.set(None);
        let Some(level) = self.level() else {
            self.chain().cancel_open(child);
            return;
        };
        if self.inner.last_hovered.get() != Some(row)
            || !self
                .inner
                .panel
                .state(app)
                .is_some_and(PopupState::is_dismissable)
            || self.chain().state(level) != Some(LevelState::Open)
        {
            self.chain().cancel_open(child);
            return;
        }
        if self
            .open_submenu_row(app, row, index, OpenSource::Pointer, Some(child))
            .is_err()
        {
            self.chain().cancel_open(child);
        }
    }

    fn cancel_pending_open(&self, app: &mut App) -> bool {
        if let Some(timer) = self.inner.pending_timer.take() {
            app.cancel_timer(&timer);
        }
        let Some(child) = self.inner.pending_open.take() else {
            return false;
        };
        self.chain().cancel_open(child);
        true
    }

    fn cancel_pending_opens(&self, app: &mut App) -> bool {
        let mut changed = self.cancel_pending_open(app);
        let submenus: Vec<Menu> = self
            .inner
            .submenus
            .borrow()
            .iter()
            .map(|open| open.menu.clone())
            .collect();
        for submenu in submenus {
            changed |= submenu.cancel_pending_opens(app);
        }
        changed
    }

    fn cancel_pending_close(&self, app: &mut App) -> bool {
        if let Some(timer) = self.inner.close_timer.take() {
            app.cancel_timer(&timer);
        }
        self.inner.closing_root.set(None);
        let chain = self.chain();
        let mut changed = false;
        for level in chain.closing_levels() {
            changed |= chain.reopen(level);
        }
        changed
    }

    fn pointer_left(&self, app: &mut App, window: WindowId) -> Result<bool, EngineError> {
        let mut changed = self.cancel_pending_opens(app);
        if self.inner.close_timer.get().is_some() || !self.chain().pointer_entered() {
            return Ok(changed);
        }
        let chain = self.chain();
        let Some(boundary) = chain.deepest_kept() else {
            return Ok(changed);
        };
        let boundary_depth = chain.depth(boundary);
        let to_close: Vec<LevelId> = chain
            .open_levels()
            .into_iter()
            .filter(|level| chain.depth(*level) > boundary_depth)
            .collect();
        if to_close.is_empty() {
            return Ok(changed);
        }
        for level in &to_close {
            changed |= chain.mark_closing(*level);
        }
        let Some(shallowest) = to_close.into_iter().min_by_key(|level| chain.depth(*level)) else {
            return Ok(changed);
        };
        self.inner.closing_root.set(Some(shallowest));
        let delay = chain.timings().close_grace;
        let this = self.clone();
        let timer = app.after(window, delay, move |app| {
            this.fire_close_grace(app);
        })?;
        self.inner.close_timer.set(Some(timer));
        Ok(changed)
    }

    fn fire_close_grace(&self, app: &mut App) {
        self.inner.close_timer.set(None);
        let Some(level) = self.inner.closing_root.take() else {
            return;
        };
        let Some(window) = self.inner.window.get() else {
            return;
        };
        if self
            .chain_pointer(app)
            .is_some_and(|point| self.chain().contains(point))
        {
            self.cancel_pending_close(app);
            return;
        }
        let Some(parent) = self.chain().parent(level) else {
            return;
        };
        let row = self.chain().row(level).unwrap_or(0);
        if let Some(parent_menu) = self.inner.sink.owner(parent) {
            parent_menu.close_submenu_row(app, row);
        }
        let _ = app.request_repaint(window);
    }

    fn open_submenu_at(&self, app: &App, row: usize) -> Option<Menu> {
        self.inner
            .submenus
            .borrow()
            .iter()
            .find(|open| {
                open.row == row
                    && open
                        .menu
                        .inner
                        .panel
                        .state(app)
                        .is_some_and(PopupState::is_dismissable)
            })
            .map(|open| open.menu.clone())
    }

    fn open_submenu_row(
        &self,
        app: &mut App,
        row: usize,
        index: usize,
        source: OpenSource,
        pending: Option<LevelId>,
    ) -> Result<Option<Menu>, PopupError> {
        if let Some(open) = self.open_submenu_at(app, row) {
            if !source.is_pointer() {
                open.focus(app);
            }
            return Ok(Some(open));
        }
        let Some(level) = self.level() else {
            return Ok(None);
        };
        let entries = self.inner.entries.borrow().clone();
        let Some(MenuEntry::Submenu(submenu)) = entries.get(index) else {
            return Ok(None);
        };
        if submenu.disabled {
            return Ok(None);
        }
        let (Some(window), Some(row_node)) = (
            self.inner.window.get(),
            self.inner.panel.list().row_node(app, row),
        ) else {
            return Ok(None);
        };
        let row_rect = app
            .window(window)
            .and_then(|target| target.node_screen_rect(row_node));
        if let Some(timer) = self.inner.pending_timer.take() {
            app.cancel_timer(&timer);
        }
        self.inner.pending_open.set(None);
        self.close_submenus(app);
        let child_level = match pending {
            Some(pending) => pending,
            None => self.chain().begin_open(level, row, source),
        };
        if let Some(row_rect) = row_rect {
            self.chain().set_anchor(child_level, row_rect);
        }
        let mut options = self.inner.options.clone();
        let mut placement = PlacementSpec::submenu();
        placement.gap = SUBMENU_GAP;
        options.placement = placement;
        let child = Menu::build(
            app,
            format_smolstr!("{}:sub:{row}", self.inner.key),
            submenu.entries.clone(),
            MenuSeed {
                theme: self.inner.theme.clone(),
                options,
                kind: MenuKind::Submenu,
                parent: Some((Rc::downgrade(&self.inner), row)),
                a11y_parent: Some(row_node),
                sink: self.inner.sink.clone(),
                level: Some(child_level),
            },
        );
        let anchor = Anchor::Node { node: row_node };
        match child.open_focused(app, window, anchor, !source.is_pointer(), source) {
            Ok(true) => {
                self.inner.submenus.borrow_mut().push(OpenSubmenu {
                    row,
                    level: child_level,
                    menu: child.clone(),
                });
                if let Some(pointer) = app.popup_pointer(window) {
                    self.chain().set_exit(child_level, pointer);
                }
                self.chain().compute_grace(child_level);
                self.arm_grace_timer(app, child_level)
                    .map_err(|source| popup_error(app, window, source))?;
                self.inner.trigger_over.set(true);
                Ok(Some(child))
            }
            Ok(false) => {
                self.chain().cancel_open(child_level);
                let _ = child.release(app);
                Ok(None)
            }
            Err(error) => {
                self.chain().cancel_open(child_level);
                let _ = child.release(app);
                Err(error)
            }
        }
    }

    fn close_submenu_row(&self, app: &mut App, row: usize) -> bool {
        let Some(open) = self.take_submenu(app, row) else {
            return false;
        };

        self.chain().close(open.level);
        let closed = open.menu.close_now(app);
        let _ = open.menu.release(app);
        closed
    }

    fn take_submenu(&self, app: &App, row: usize) -> Option<OpenSubmenu> {
        let mut submenus = self.inner.submenus.borrow_mut();
        let index = submenus.iter().position(|open| {
            open.row == row
                && open
                    .menu
                    .inner
                    .panel
                    .state(app)
                    .is_some_and(PopupState::is_dismissable)
        })?;
        Some(submenus.remove(index))
    }

    fn close_submenus(&self, app: &mut App) -> bool {
        let mut changed = self.cancel_pending_open(app);
        if let Some(timer) = self.inner.grace_timer.take() {
            app.cancel_timer(&timer);
        }
        self.inner.trigger_over.set(false);
        let submenus: Vec<OpenSubmenu> = self.inner.submenus.borrow_mut().drain(..).collect();
        for open in submenus.into_iter().rev() {
            self.chain().close(open.level);
            open.menu.close_now(app);
            let _ = open.menu.release(app);
            changed = true;
        }
        changed
    }

    fn entry_index(&self, row: usize) -> Option<usize> {
        let kind = self.row_kind(row)?;
        match kind {
            RowKind::Action(index) | RowKind::Submenu(index) | RowKind::Group(index) => Some(index),
            RowKind::Separator => None,
        }
    }

    fn row_kind(&self, row: usize) -> Option<RowKind> {
        self.inner.rows.borrow().get(row).copied()
    }

    fn index_row(&self, index: usize) -> Option<usize> {
        self.inner.rows.borrow().iter().position(|kind| match kind {
            RowKind::Action(row) | RowKind::Submenu(row) | RowKind::Group(row) => *row == index,
            RowKind::Separator => index == usize::MAX,
        })
    }

    fn focus(&self, app: &mut App) -> bool {
        self.inner.panel.focus(app)
    }

    fn keyboard_target(&self, app: &mut App) -> Menu {
        let target = self
            .chain()
            .open_levels()
            .into_iter()
            .rev()
            .find_map(|level| {
                let menu = self.inner.sink.owner(level)?;
                menu.inner.panel.is_open(app).then_some(menu)
            });
        let Some(menu) = target else {
            return self.clone();
        };
        if menu.level() != self.level() {
            menu.focus(app);
        }
        menu
    }

    fn activate_action(&self, app: &mut App, row: usize, index: usize, keep_open: bool) {
        let entries = self.inner.entries.borrow().clone();
        let Some(MenuEntry::Action(action)) = entries.get(index) else {
            return;
        };
        if action.disabled {
            return;
        }
        if action.check.is_some() {
            self.toggle_checked(app, row, index, keep_open);
            return;
        }
        let activation = MenuActivation {
            id: action.id.clone(),
            check: None,
        };
        if let Some(callback) = self.inner.sink.on_activate.borrow().clone() {
            callback(app, activation);
        }
        self.close_chain(app, DismissReason::Programmatic);
    }

    fn toggle_checked(&self, app: &mut App, row: usize, index: usize, keep_open: bool) {
        let entries = self.inner.entries.borrow().clone();
        let Some(MenuEntry::Action(action)) = entries.get(index) else {
            return;
        };
        let Some(check) = action.check else {
            return;
        };
        let checked = match check.kind {
            MenuCheck::Checkbox => !check.checked,
            MenuCheck::Radio => true,
        };
        let mut updated: Vec<MenuEntry> = entries.as_ref().clone();
        if check.kind == MenuCheck::Radio {
            for run_row in self.radio_run(row) {
                let Some(RowKind::Action(run_index)) = self.row_kind(run_row) else {
                    continue;
                };
                if let Some(MenuEntry::Action(other)) = updated.get_mut(run_index)
                    && let Some(other_check) = &mut other.check
                    && other_check.kind == MenuCheck::Radio
                {
                    other_check.checked = false;
                }
            }
        }
        if let Some(MenuEntry::Action(target)) = updated.get_mut(index)
            && let Some(target_check) = &mut target.check
        {
            target_check.checked = checked;
        }
        *self.inner.entries.borrow_mut() = Rc::new(updated);
        self.install_content(app);
        let activation = MenuActivation {
            id: action.id.clone(),
            check: Some(MenuCheckState {
                kind: check.kind,
                checked,
            }),
        };
        if let Some(callback) = self.inner.sink.on_activate.borrow().clone() {
            callback(app, activation);
        }
        if !keep_open {
            self.close_chain(app, DismissReason::Programmatic);
        }
    }

    fn radio_run(&self, row: usize) -> Vec<usize> {
        let entries = self.inner.entries.borrow().clone();
        let is_radio = |row: usize| {
            self.entry_index(row).is_some_and(|index| {
                matches!(
                    entries.get(index),
                    Some(MenuEntry::Action(action))
                        if action.check.is_some_and(|check| check.kind == MenuCheck::Radio)
                )
            })
        };
        let mut start = row;
        while start > 0 && is_radio(start - 1) {
            start -= 1;
        }
        let mut end = row;
        while end + 1 < self.row_count() && is_radio(end + 1) {
            end += 1;
        }
        (start..=end).collect()
    }

    fn handle_row(&self, app: &mut App, row: usize, keep_open: bool) {
        match self.row_kind(row) {
            Some(RowKind::Submenu(index)) => {
                let _ = self.open_submenu_row(app, row, index, OpenSource::Click, None);
            }
            Some(RowKind::Action(index)) => {
                self.activate_action(app, row, index, keep_open);
            }
            Some(RowKind::Group(_) | RowKind::Separator) | None => {}
        }
    }

    fn activate_index(&self, app: &mut App, index: usize, keep_open: bool) -> bool {
        if self.row_disabled(index) || index >= self.row_count() {
            return false;
        }
        let Some(row) = self.index_row(index) else {
            return false;
        };
        self.set_active_row(app, Some(row));
        self.handle_row(app, row, keep_open);
        true
    }

    fn close_chain(&self, app: &mut App, reason: DismissReason) {
        let root = self.root();
        let _ = root.inner.panel.close_with_reason(app, reason);
    }

    fn page_highlight(&self, app: &mut App, direction: isize) {
        let count = self.row_count();
        if count == 0 {
            return;
        }
        let step = self.inner.panel.page_step(app) as isize * direction;
        let current = self.highlighted(app).unwrap_or(0) as isize;
        let mut candidate = (current + step).clamp(0, count as isize - 1);
        while candidate >= 0
            && (candidate as usize) < count
            && self.row_disabled(candidate as usize)
        {
            candidate += direction.signum();
            if candidate < 0 || candidate >= count as isize {
                return;
            }
        }
        self.set_active_row(app, Some(candidate as usize));
    }

    fn tab_out(&self, app: &mut App, forward: bool) {
        let root = self.root();
        let Some(window) = root.inner.window.get() else {
            return;
        };
        let opener = self.inner.opener.get();
        root.close_now(app);
        if let Some(opener) = opener {
            let _ = app.set_focus(window, Some(opener));
        }
        if forward {
            app.focus_next(window);
        } else {
            app.focus_prev(window);
        }
    }

    fn typeahead(&self, app: &mut App, character: char) {
        let Some(level) = self.level() else {
            return;
        };
        let existing = self.chain().typeahead_buffer();
        if !existing.is_empty() && existing.chars().all(|value| value == character) {
            self.chain().clear_typeahead();
        }
        let buffer = self.chain().typeahead_push(level, character);
        self.arm_typeahead_timer(app);
        if let Some(row) = self.typeahead_row(app, &buffer) {
            self.set_active_row(app, Some(row));
        }
    }

    fn arm_typeahead_timer(&self, app: &mut App) {
        if let Some(timer) = self.inner.typeahead_timer.take() {
            app.cancel_timer(&timer);
        }
        let Some(window) = self.inner.window.get() else {
            return;
        };
        let Some(level) = self.level() else {
            return;
        };
        let delay = self.chain().timings().typeahead_timeout;
        let chain = self.chain().clone();
        if let Ok(timer) = app.after(window, delay, move |_| {
            chain.clear_typeahead_for(level);
        }) {
            self.inner.typeahead_timer.set(Some(timer));
        }
    }

    fn typeahead_row(&self, app: &App, buffer: &str) -> Option<usize> {
        let buffer = buffer.to_lowercase();
        let count = self.row_count();
        if count == 0 || buffer.is_empty() {
            return None;
        }
        let entries = self.inner.entries.borrow().clone();
        let label = |row: usize| {
            self.entry_index(row)
                .and_then(|index| match entries.get(index) {
                    Some(MenuEntry::Action(action)) if !action.disabled => {
                        Some(action.label.clone())
                    }
                    Some(MenuEntry::Submenu(submenu)) if !submenu.disabled => {
                        Some(submenu.label.clone())
                    }
                    _ => None,
                })
        };
        let active = self.highlighted(app);
        let start = if buffer.chars().count() == 1 {
            active.map_or(0, |index| (index + 1) % count)
        } else {
            active.unwrap_or(0)
        };
        (0..count)
            .map(|offset| (start + offset) % count)
            .find(|row| label(*row).is_some_and(|text| text.to_lowercase().starts_with(&buffer)))
    }

    fn sync_active_row(&self, app: &mut App) -> bool {
        let Some(level) = self.level() else {
            return false;
        };
        let row = self.highlighted(app);
        let changed = self.chain().set_active(level, row);
        let a11y_changed = self.sync_active_descendant(app);
        changed || a11y_changed
    }

    fn install_content(&self, app: &mut App) {
        let entries = self.inner.entries.borrow().clone();
        let mut rows = Vec::with_capacity(entries.len());
        let mut kinds = Vec::with_capacity(entries.len());
        let size = self.inner.options.size;
        let mut has_icon = false;
        for (index, entry) in entries.iter().enumerate() {
            match entry {
                MenuEntry::Action(action) => {
                    has_icon |= action.icon.is_some();
                    rows.push(PanelRow {
                        height: size.h(),
                        disabled: action.disabled,
                    });
                    kinds.push(RowKind::Action(index));
                }
                MenuEntry::Submenu(submenu) => {
                    has_icon |= submenu.icon.is_some();
                    rows.push(PanelRow {
                        height: size.h(),
                        disabled: submenu.disabled,
                    });
                    kinds.push(RowKind::Submenu(index));
                }
                MenuEntry::Group(_) => {
                    rows.push(PanelRow {
                        height: size.h() * GROUP_HEIGHT_RATIO,
                        disabled: true,
                    });
                    kinds.push(RowKind::Group(index));
                }
                MenuEntry::Separator => {
                    rows.push(PanelRow {
                        height: SEPARATOR_HEIGHT,
                        disabled: true,
                    });
                    kinds.push(RowKind::Separator);
                }
            }
        }
        *self.inner.rows.borrow_mut() = kinds;
        let theme = self.inner.theme.clone();
        let icons = Rc::new(self.icon_snapshot());
        let entries_build = entries.clone();
        let build: Rc<dyn Fn(&mut NodeTree, NodeId, usize) -> Result<(), EngineError>> =
            Rc::new(move |tree, parent, index| {
                let Some(entry) = entries_build.get(index) else {
                    return Ok(());
                };
                let text_style = TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap);
                match entry {
                    MenuEntry::Action(action) => {
                        build_icon_slot(tree, parent, &theme, &icons, action.icon, size, has_icon)?;
                        build_label(
                            tree,
                            parent,
                            &theme,
                            &action.label,
                            text_style,
                            action_foreground(action),
                        )?;
                        if let Some(shortcut) = &action.shortcut {
                            build_shortcut(tree, parent, &theme, shortcut, size)?;
                        }
                        if let Some(check) = action.check {
                            build_check(tree, parent, &theme, check.kind, check.checked, size)?;
                        }
                        Ok(())
                    }
                    MenuEntry::Submenu(submenu) => {
                        build_icon_slot(
                            tree,
                            parent,
                            &theme,
                            &icons,
                            submenu.icon,
                            size,
                            has_icon,
                        )?;
                        let foreground = if submenu.disabled {
                            Role::FgDisabled
                        } else {
                            Role::FgDefault
                        };
                        build_label(tree, parent, &theme, &submenu.label, text_style, foreground)?;
                        if let Some(resource) = icons.get(IconKind::CaretRight) {
                            let theme_icon = theme.clone();
                            build_icon(tree, parent, resource, size.icon_size(), move |cx| {
                                StylePatch {
                                    foreground: Some(theme_icon.role(cx, Role::FgMuted)),
                                    ..StylePatch::default()
                                }
                            })?;
                        }
                        Ok(())
                    }
                    MenuEntry::Group(label) => {
                        build_label(tree, parent, &theme, label, text_style, Role::FgMuted)
                    }
                    MenuEntry::Separator => {
                        let theme_line = theme.clone();
                        NodeBuilder::boxed()
                            .style(StyleBuilder::new().width_full().height(px(1.0)).build())
                            .bind_style(move |cx| StylePatch {
                                background: Some(theme_line.role(cx, Role::LineDivider)),
                                ..StylePatch::default()
                            })
                            .a11y(A11yOverride {
                                hidden: true,
                                ..A11yOverride::default()
                            })
                            .build(tree, parent)
                            .map(|_| ())
                    }
                }
            });
        let entries_a11y = entries;
        let a11y: Rc<dyn Fn(usize) -> A11yPatch> = Rc::new(move |index| {
            let Some(entry) = entries_a11y.get(index) else {
                return A11yPatch {
                    hidden: Some(true),
                    ..A11yPatch::default()
                };
            };
            match entry {
                MenuEntry::Action(action) => A11yPatch {
                    role: Some(match action.check.map(|check| check.kind) {
                        Some(MenuCheck::Checkbox) => A11yRole::MenuItemCheckBox,
                        Some(MenuCheck::Radio) => A11yRole::MenuItemRadio,
                        None => MENU_ROLES.row,
                    }),
                    label: Some(action.label.clone()),
                    disabled: Some(action.disabled),
                    checked: action.check.map(|check| check.checked),
                    ..A11yPatch::default()
                },
                MenuEntry::Submenu(submenu) => A11yPatch {
                    role: Some(MENU_ROLES.row),
                    label: Some(submenu.label.clone()),
                    disabled: Some(submenu.disabled),
                    has_popup: Some(HasPopup::Menu),
                    expanded: Some(false),
                    ..A11yPatch::default()
                },
                MenuEntry::Group(label) => A11yPatch {
                    role: Some(A11yRole::Group),
                    label: Some(label.clone()),
                    ..A11yPatch::default()
                },
                MenuEntry::Separator => A11yPatch {
                    role: Some(A11yRole::Separator),
                    hidden: Some(true),
                    ..A11yPatch::default()
                },
            }
        });
        let _ = self.inner.panel.set_content(
            app,
            PanelContent {
                rows,
                build,
                a11y,
                measure: None,
            },
        );
    }

    fn install_handlers(&self, app: &mut App) {
        let weak = Rc::downgrade(&self.inner);
        self.inner.panel.on_dismiss(move |app, dismiss| {
            let Some(inner) = weak.upgrade() else {
                return DismissOutcome::Close;
            };
            Menu { inner }.dismiss_outcome(app, dismiss)
        });
        let this = self.clone();
        self.inner.panel.on_activate(move |app, index| {
            this.activate_index(app, index, false);
        });
        let this = self.clone();
        self.inner.panel.on_close(move |app, reason| {
            this.close_submenus(app);
            if let Some(level) = this.level() {
                let parent = this.chain().parent(level);
                let row = this.chain().row(level).unwrap_or(0);
                this.chain().close(level);
                if let Some(parent) = parent.and_then(|parent| this.inner.sink.owner(parent)) {
                    let pointer = this
                        .inner
                        .window
                        .get()
                        .and_then(|window| app.popup_pointer(window));
                    let over_row =
                        pointer.is_some_and(|point| parent.row_contains(app, row, point));

                    if !over_row && parent.inner.last_hovered.get() == Some(row) {
                        parent.inner.last_hovered.set(None);
                        parent.inner.trigger_over.set(false);
                    }
                    parent.set_active_row(app, Some(row));
                }
            }
            this.fire_close(app, reason);
        });
        self.register_key_actions(app);
    }

    fn register_key_actions(&self, app: &mut App) {
        for (action, step) in [
            (self.inner.up_action.clone(), -1isize),
            (self.inner.down_action.clone(), 1isize),
        ] {
            let this = self.clone();
            app.on_action(action, move |ctx| {
                let app = ctx.app_mut();
                let target = this.keyboard_target(app);
                target.inner.panel.move_highlight(app, step);
                target.sync_active_row(app);
            });
        }
        for (action, last) in [
            (self.inner.home_action.clone(), false),
            (self.inner.end_action.clone(), true),
        ] {
            let this = self.clone();
            app.on_action(action, move |ctx| {
                let app = ctx.app_mut();
                let target = this.keyboard_target(app);
                target.inner.panel.jump_highlight(app, last);
                target.sync_active_row(app);
            });
        }
        for (action, direction) in [
            (self.inner.page_up_action.clone(), -1isize),
            (self.inner.page_down_action.clone(), 1isize),
        ] {
            let this = self.clone();
            app.on_action(action, move |ctx| {
                let app = ctx.app_mut();
                let target = this.keyboard_target(app);
                target.page_highlight(app, direction);
                target.sync_active_row(app);
            });
        }
        let this = self.clone();
        app.on_action(self.inner.surface_action.clone(), move |ctx| {
            let window = ctx.window();
            let app = ctx.app_mut();
            let target = this.keyboard_target(app);
            let keep_open =
                target.highlighted_checkable(app) && app.key_down(window, KeyCode::Space);
            target.key_activate(app, keep_open);
        });
        let this = self.clone();
        app.on_action(self.inner.right_action.clone(), move |ctx| {
            let app = ctx.app_mut();
            let target = this.keyboard_target(app);
            target.key_open_submenu(app);
        });
        let this = self.clone();
        app.on_action(self.inner.left_action.clone(), move |ctx| {
            let app = ctx.app_mut();
            let target = this.keyboard_target(app);
            target.close_to_parent(app);
        });
        let this = self.clone();
        app.on_action(self.inner.tab_next_action.clone(), move |ctx| {
            let app = ctx.app_mut();
            let target = this.keyboard_target(app);
            target.tab_out(app, true);
        });
        let this = self.clone();
        app.on_action(self.inner.tab_prev_action.clone(), move |ctx| {
            let app = ctx.app_mut();
            let target = this.keyboard_target(app);
            target.tab_out(app, false);
        });
        for (character, action) in self.inner.typeahead_actions.borrow().iter() {
            let character = *character;
            let this = self.clone();
            app.on_action(action.clone(), move |ctx| {
                let app = ctx.app_mut();
                let target = this.keyboard_target(app);
                target.typeahead(app, character);
            });
        }
    }

    fn key_open_submenu(&self, app: &mut App) {
        if let Some(row) = self.highlighted(app)
            && let Some(RowKind::Submenu(index)) = self.row_kind(row)
        {
            let _ = self.open_submenu_row(app, row, index, OpenSource::Keyboard, None);
        }
    }

    fn key_activate(&self, app: &mut App, keep_open: bool) {
        let Some(row) = self.highlighted(app) else {
            return;
        };
        match self.row_kind(row) {
            Some(RowKind::Submenu(index)) => {
                let _ = self.open_submenu_row(app, row, index, OpenSource::Keyboard, None);
            }
            Some(RowKind::Action(index)) => self.activate_action(app, row, index, keep_open),
            Some(RowKind::Group(_) | RowKind::Separator) | None => {}
        }
    }

    fn highlighted_checkable(&self, app: &App) -> bool {
        let Some(row) = self.highlighted(app) else {
            return false;
        };
        let Some(RowKind::Action(index)) = self.row_kind(row) else {
            return false;
        };
        let entries = self.inner.entries.borrow();
        matches!(
            entries.get(index),
            Some(MenuEntry::Action(action)) if action.check.is_some() && !action.disabled
        )
    }

    fn close_to_parent(&self, app: &mut App) {
        if self.inner.kind != MenuKind::Submenu {
            return;
        }
        let Some(level) = self.level() else {
            return;
        };
        let Some(window) = self.inner.window.get() else {
            return;
        };
        let parent = self.chain().parent(level);
        let row = self.chain().row(level).unwrap_or(0);
        self.close_now(app);
        if let Some(parent) = parent.and_then(|parent| self.inner.sink.owner(parent)) {
            parent.set_active_row(app, Some(row));
            parent.focus(app);
        }
        let _ = app.request_repaint(window);
    }

    fn fire_close(&self, app: &mut App, reason: DismissReason) {
        if self.inner.kind != MenuKind::Root {
            return;
        }
        if let Some(callback) = self.inner.sink.on_close.borrow().clone() {
            callback(app, reason);
        }
    }

    fn root(&self) -> Menu {
        let Some((parent, _)) = self.inner.parent.as_ref() else {
            return self.clone();
        };
        match parent.upgrade() {
            Some(parent) => Menu { inner: parent }.root(),
            None => self.clone(),
        }
    }

    fn retain_open_submenus(&self, app: &mut App) {
        let mut stale: Vec<OpenSubmenu> = Vec::new();
        {
            let mut submenus = self.inner.submenus.borrow_mut();
            let mut index = 0;
            while index < submenus.len() {
                if submenus[index].menu.inner.panel.state(app).is_some() {
                    index += 1;
                } else {
                    stale.push(submenus.remove(index));
                }
            }
        }
        for open in stale {
            self.chain().close(open.level);
            let _ = open.menu.release(app);
        }
    }

    fn sync_anchor(&self, app: &mut App) -> bool {
        let (Some(id), Some(window)) = (self.inner.panel.id(), self.inner.window.get()) else {
            return false;
        };
        let animating = self.ancestors_animating(app);
        let was_animating = self.inner.watch_animation.replace(animating);
        let change = self.inner.watch.borrow_mut().observe(app, id, window);
        let internal_scroll = self.sync_internal_scroll(app);
        let change = if change == AnchorChange::Moved && (animating || was_animating) {
            AnchorChange::None
        } else {
            change
        };

        match change {
            AnchorChange::Lost | AnchorChange::Resized | AnchorChange::Moved => {
                self.close_chain(app, DismissReason::AnchorLost);
                true
            }
            AnchorChange::Scrolled if self.inner.scroll_closes.get() && !internal_scroll => {
                self.close_chain(app, DismissReason::AnchorLost);
                true
            }
            AnchorChange::None | AnchorChange::Scrolled => false,
        }
    }

    fn ancestors_animating(&self, app: &App) -> bool {
        let Some(window) = self.inner.window.get() else {
            return false;
        };
        let Some(stack) = app.popup_stack(window) else {
            return false;
        };
        let chain = self.chain();
        let mut level = self.level();
        while let Some(parent) = level.and_then(|level| chain.parent(level)) {
            if let Some(menu) = self.inner.sink.owner(parent)
                && let Some(id) = menu.inner.panel.id()
                && stack.entries.iter().any(|entry| {
                    entry.id == id
                        && (entry.paint_offset != LogicalPoint::ZERO || entry.paint_scale != 1.0)
                })
            {
                return true;
            }
            level = Some(parent);
        }
        false
    }

    fn sync_internal_scroll(&self, app: &App) -> bool {
        let offset = self.inner.panel.list().offset(app);
        let changed = offset.is_some() && offset != self.inner.internal_scroll.get();
        self.inner.internal_scroll.set(offset);
        changed
    }

    fn bind_extra_keys(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let Some(surface) = self.inner.panel.surface_node() else {
            return Ok(());
        };
        let mut bindings: Vec<(KeyCode, Modifiers, Action)> = vec![
            (
                KeyCode::ArrowRight,
                Modifiers::empty(),
                self.inner.right_action.clone(),
            ),
            (
                KeyCode::ArrowLeft,
                Modifiers::empty(),
                self.inner.left_action.clone(),
            ),
            (
                KeyCode::ArrowUp,
                Modifiers::empty(),
                self.inner.up_action.clone(),
            ),
            (
                KeyCode::ArrowDown,
                Modifiers::empty(),
                self.inner.down_action.clone(),
            ),
            (
                KeyCode::Home,
                Modifiers::empty(),
                self.inner.home_action.clone(),
            ),
            (
                KeyCode::End,
                Modifiers::empty(),
                self.inner.end_action.clone(),
            ),
            (
                KeyCode::PageUp,
                Modifiers::empty(),
                self.inner.page_up_action.clone(),
            ),
            (
                KeyCode::PageDown,
                Modifiers::empty(),
                self.inner.page_down_action.clone(),
            ),
            (
                KeyCode::Tab,
                Modifiers::empty(),
                self.inner.tab_next_action.clone(),
            ),
            (
                KeyCode::Tab,
                Modifiers::SHIFT,
                self.inner.tab_prev_action.clone(),
            ),
        ];
        let actions = self.inner.typeahead_actions.borrow();
        for (key, character) in TYPEAHEAD_KEYS {
            let Some((_, action)) = actions.iter().find(|(value, _)| *value == character) else {
                continue;
            };
            bindings.push((key, Modifiers::empty(), action.clone()));
            if character.is_ascii_alphabetic() {
                bindings.push((key, Modifiers::SHIFT, action.clone()));
            }
        }
        drop(actions);
        for (key, modifiers, action) in bindings {
            app.bind_key_scoped(
                window,
                surface,
                KeyBinding {
                    key,
                    modifiers,
                    action,
                },
            )?;
        }
        Ok(())
    }

    fn icon_snapshot(&self) -> IconSnapshot {
        self.inner.icons.borrow().clone()
    }

    fn resolve_icons(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let entries = self.inner.entries.borrow().clone();
        let mut icons = self.inner.icons.borrow_mut();
        for entry in entries.iter() {
            let icon = match entry {
                MenuEntry::Action(action) => action.icon,
                MenuEntry::Submenu(submenu) => submenu.icon,
                MenuEntry::Group(_) | MenuEntry::Separator => None,
            };
            if let Some(kind) = icon {
                icons.resolve(app, window, kind)?;
            }
        }
        if entries
            .iter()
            .any(|entry| matches!(entry, MenuEntry::Submenu(_)))
        {
            icons.resolve(app, window, IconKind::CaretRight)?;
        }
        Ok(())
    }

    fn row_of(&self, id: &str) -> Option<usize> {
        let entries = self.inner.entries.borrow().clone();
        let rows = self.inner.rows.borrow();
        rows.iter().enumerate().find_map(|(row, kind)| {
            let index = match kind {
                RowKind::Action(index) | RowKind::Submenu(index) | RowKind::Group(index) => *index,
                RowKind::Separator => return None,
            };
            match entries.get(index) {
                Some(MenuEntry::Action(action)) if action.id == id => Some(row),
                _ => None,
            }
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct IconSnapshot {
    entries: Vec<(IconKind, ResourceId)>,
}

impl IconSnapshot {
    pub(crate) fn resolve(
        &mut self,
        app: &mut App,
        window: WindowId,
        kind: IconKind,
    ) -> Result<(), EngineError> {
        let image = app.insert_svg_for_window(window, svg(kind).as_bytes(), ICON_INTRINSIC_SIZE)?;
        let resource = image.id();
        match self
            .entries
            .iter_mut()
            .find(|(entry_kind, _)| *entry_kind == kind)
        {
            Some(entry) => entry.1 = resource,
            None => self.entries.push((kind, resource)),
        }
        Ok(())
    }

    pub(crate) fn get(&self, kind: IconKind) -> Option<ResourceId> {
        self.entries
            .iter()
            .find(|(entry_kind, _)| *entry_kind == kind)
            .map(|(_, resource)| *resource)
    }
}

pub(crate) fn popup_error(app: &App, window: WindowId, source: EngineError) -> PopupError {
    if let EngineError::UnknownWindow { window } = source {
        return PopupError::UnknownWindow { window };
    }
    match app.window(window) {
        Some(target) => PopupError::ContentBuild {
            node: target.tree().root(),
            source,
        },
        None => PopupError::StackSync { source },
    }
}

pub(crate) fn build_icon_slot(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: &ThemeHandle,
    icons: &IconSnapshot,
    icon: Option<IconKind>,
    size: UnifiedSize,
    reserve: bool,
) -> Result<(), EngineError> {
    let side = size.icon_size();
    match icons.get(icon.unwrap_or(IconKind::X)) {
        Some(resource) if icon.is_some() => {
            let theme = theme.clone();
            build_icon(tree, parent, resource, side, move |cx| StylePatch {
                foreground: Some(theme.role(cx, Role::FgMuted)),
                ..StylePatch::default()
            })
        }
        _ if reserve => NodeBuilder::boxed()
            .style(StyleBuilder::new().size(side, side).build())
            .a11y(A11yOverride {
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, parent)
            .map(|_| ()),
        _ => Ok(()),
    }
}

fn build_label(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: &ThemeHandle,
    label: &str,
    text_style: TextStyle,
    foreground: Role,
) -> Result<(), EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(label, text_style)
        .style(StyleBuilder::new().flex_1().min_width_0().build())
        .bind_style(move |cx| StylePatch {
            foreground: Some(theme.role(cx, foreground)),
            ..StylePatch::default()
        })
        .build(tree, parent)
        .map(|_| ())
}

fn build_shortcut(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: &ThemeHandle,
    shortcut: &str,
    size: UnifiedSize,
) -> Result<(), EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(
        shortcut,
        TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
    )
    .style(StyleBuilder::new().flex_none().build())
    .bind_style(move |cx| StylePatch {
        foreground: Some(theme.role(cx, Role::FgMuted)),
        ..StylePatch::default()
    })
    .build(tree, parent)
    .map(|_| ())
}

fn build_check(
    tree: &mut NodeTree,
    parent: NodeId,
    theme: &ThemeHandle,
    kind: MenuCheck,
    checked: bool,
    size: UnifiedSize,
) -> Result<(), EngineError> {
    let theme = theme.clone();
    let glyph = match (kind, checked) {
        (MenuCheck::Checkbox, true) => "✓",
        (MenuCheck::Radio, true) => "●",
        (MenuCheck::Checkbox | MenuCheck::Radio, false) => " ",
    };
    NodeBuilder::text(
        glyph,
        TextStyle::new(size.text_size()).with_wrap(TextWrap::NoWrap),
    )
    .style(StyleBuilder::new().flex_none().build())
    .bind_style(move |cx| StylePatch {
        foreground: Some(theme.role(cx, Role::FgDefault)),
        ..StylePatch::default()
    })
    .build(tree, parent)
    .map(|_| ())
}

fn action_foreground(action: &MenuAction) -> Role {
    if action.disabled {
        Role::FgDisabled
    } else if action.danger {
        Role::StatusDangerBase
    } else {
        Role::FgDefault
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
        interaction::ScrollConfig,
        math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::style_builder::StyleBuilder,
        popup::{
            Anchor, DismissReason, HasPopup, OutsidePress, PopupId, PopupSnapshotEntry, PopupState,
        },
        test_support::{app::TestApp, record::FrameRecord},
        window::{WindowId, WindowOptions},
    };

    use super::{
        Menu, MenuAction, MenuActivation, MenuCheck, MenuCheckState, MenuEntry, MenuOptions,
        MenuSubmenu,
    };
    use crate::{
        kit::test_support::full_a11y,
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    const ANCHOR: LogicalRect = LogicalRect::from_origin_size(
        LogicalPoint::new(px(300.0), px(200.0)),
        LogicalSize::new(px(120.0), px(32.0)),
    );

    struct Harness {
        app: TestApp,
        window: WindowId,
        theme: ThemeHandle,
        anchor: NodeId,
        background: NodeId,
        activated: Rc<RefCell<Vec<SmolStr>>>,
        closes: Rc<Cell<usize>>,
        background_clicks: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            Self::sized(LogicalSize::new(px(800.0), px(600.0)))
        }

        fn sized(size: LogicalSize) -> Self {
            let mut app = TestApp::with_options(WindowOptions {
                size,
                ..WindowOptions::default()
            });
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let anchor = NodeBuilder::boxed()
                .id("anchor")
                .layout_override(ANCHOR)
                .build(
                    app.app_mut().window_mut(window).expect("窗口").tree_mut(),
                    root,
                )
                .expect("锚点");
            let clicks = Rc::new(Cell::new(0));
            let sink = clicks.clone();
            app.app_mut().on_action(
                Action::Custom(SmolStr::new_static("background.click")),
                move |_| sink.set(sink.get() + 1),
            );
            let background = NodeBuilder::boxed()
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
                anchor,
                background,
                activated: Rc::new(RefCell::new(Vec::new())),
                closes: Rc::new(Cell::new(0)),
                background_clicks: clicks,
            }
        }

        fn menu(&mut self, entries: Vec<MenuEntry>, options: MenuOptions) -> Menu {
            let activated = self.activated.clone();
            let closes = self.closes.clone();
            Menu::new(
                self.app.app_mut(),
                self.theme.clone(),
                "menu",
                entries,
                options,
            )
            .on_activate(move |_, activation| activated.borrow_mut().push(activation.id))
            .on_close(move |_, _| closes.set(closes.get() + 1))
        }

        fn anchor(&self) -> Anchor {
            Anchor::Node { node: self.anchor }
        }

        fn open(&mut self, menu: &Menu) -> bool {
            let anchor = self.anchor();
            let opened = menu
                .open(self.app.app_mut(), self.window, anchor)
                .expect("打开菜单");
            let _ = menu.sync(self.app.app_mut());
            self.drain(menu);
            opened
        }

        fn drain(&mut self, menu: &Menu) -> FrameRecord {
            let record = self.app.drain().expect("帧").clone();
            let _ = menu.sync(self.app.app_mut());
            record
        }

        fn settle(&mut self, menu: &Menu) {
            for _ in 0..6 {
                self.app.advance(Duration::from_millis(40));
                self.drain(menu);
            }
            self.app.advance(Duration::from_millis(200));
            self.drain(menu);
        }

        fn key(&mut self, menu: &Menu, code: KeyCode) {
            let _ = self
                .app
                .inject(InputEvent::key_down(code, Modifiers::empty()));
            let _ = self
                .app
                .inject(InputEvent::key_up(code, Modifiers::empty()));
            self.drain(menu);
        }

        fn click(&mut self, menu: &Menu, at: LogicalPoint) {
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
            self.drain(menu);
        }

        fn hover(&mut self, menu: &Menu, at: LogicalPoint) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(at, Modifiers::empty()));
            self.drain(menu);
        }

        fn advance(&mut self, menu: &Menu, duration: Duration) {
            self.app.advance(duration);
            self.drain(menu);
        }

        fn bounds(&self, node: NodeId) -> LogicalRect {
            self.app.probe().bounds(node).expect("布局盒")
        }

        fn screen_bounds(&self, node: NodeId) -> LogicalRect {
            self.app
                .app()
                .window(self.window)
                .and_then(|target| target.node_screen_rect(node))
                .expect("屏幕盒")
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
            self.find(&format!("menu:panel:list:row:{index}"))
                .unwrap_or_else(|| panic!("第 {index} 行未挂载"))
        }

        fn row_center(&self, index: usize) -> LogicalPoint {
            let bounds = self.bounds(self.row(index));
            LogicalPoint::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        fn stack(&self) -> Vec<PopupSnapshotEntry> {
            self.app
                .app()
                .popup_stack(self.window)
                .expect("浮层栈")
                .entries
        }

        fn root_entry(&self, menu: &Menu) -> PopupSnapshotEntry {
            let node = menu.popup_node().expect("浮层内容节点");
            self.stack()
                .into_iter()
                .find(|entry| entry.node == node)
                .expect("根菜单在栈里")
        }

        fn popup_rect(&self, menu: &Menu) -> LogicalRect {
            let id = self.root_entry(menu).id;
            self.app.app().popup_rect(id).expect("浮层 rect").hit
        }

        fn submenu_entry(&self, root: PopupId) -> PopupSnapshotEntry {
            self.stack()
                .into_iter()
                .find(|entry| entry.parent == Some(root))
                .expect("子菜单在栈里")
        }

        fn text_exists(&self, text: &str) -> bool {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut stack = vec![tree.root()];
            while let Some(node) = stack.pop() {
                if let Ok(target) = tree.get(node) {
                    if matches!(target.kind(), ui_core::node::tree::NodeKind::Text(content) if content.text == text)
                    {
                        return true;
                    }
                    stack.extend(target.children().iter().rev().copied());
                }
            }
            false
        }

        fn focused_debug_id(&self) -> Option<String> {
            let focused = self.app.app().focused(self.window)?;
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            tree.debug_id(focused).map(str::to_owned)
        }

        fn idles(&mut self, menu: &Menu) -> bool {
            let rendered = self.app.drain().expect("帧").rendered;
            !rendered && !menu.sync(self.app.app_mut()).expect("同步")
        }

        fn center_of(&self, debug_id: &str) -> LogicalPoint {
            let node = self
                .find(debug_id)
                .unwrap_or_else(|| panic!("{debug_id} 未挂载"));
            let bounds = self.screen_bounds(node);
            LogicalPoint::new(
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            )
        }

        fn hover_point(&mut self, menu: &Menu, point: LogicalPoint) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(point, Modifiers::empty()));
            self.drain(menu);
        }

        fn key_with(&mut self, menu: &Menu, code: KeyCode, modifiers: Modifiers) {
            let _ = self.app.inject(InputEvent::key_down(code, modifiers));
            let _ = self.app.inject(InputEvent::key_up(code, modifiers));
            self.drain(menu);
        }

        fn focus(&mut self, menu: &Menu, node: NodeId) {
            let _ = menu.inner.panel.focus(self.app.app_mut());
            assert_eq!(
                self.app.app().focused(self.window),
                Some(node),
                "焦点必须在 {node:?}"
            );
        }

        fn submenu_entry_opt(&self, root: PopupId) -> Option<PopupSnapshotEntry> {
            self.stack()
                .into_iter()
                .find(|entry| entry.parent == Some(root))
        }

        fn hover_row_of(&mut self, menu: &Menu, prefix: &str, index: usize) {
            let at = self.center_of(&format!("{prefix}:list:row:{index}"));
            self.hover_point(menu, at);
        }
    }

    fn entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Group(SmolStr::new_static("账户")),
            MenuEntry::Action(MenuAction::new("profile", "资料")),
            MenuEntry::Action(MenuAction::new("blocked", "已禁用").disabled(true)),
            MenuEntry::Separator,
            MenuEntry::Action(
                MenuAction::new("delete", "删除")
                    .danger(true)
                    .shortcut("Del"),
            ),
        ]
    }

    fn submenu_entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("a", "A")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [MenuEntry::Action(MenuAction::new("inner", "内部"))],
            )),
        ]
    }

    fn nested_submenu_entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("a", "A")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [
                    MenuEntry::Action(MenuAction::new("inner", "内部")),
                    MenuEntry::Submenu(MenuSubmenu::new(
                        "更深",
                        [MenuEntry::Action(MenuAction::new("deep", "深处"))],
                    )),
                ],
            )),
        ]
    }

    fn options() -> MenuOptions {
        MenuOptions::default()
    }

    #[test]
    fn the_menu_opens_below_its_anchor_at_its_stated_width() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        assert!(harness.open(&menu));
        assert!(menu.is_open(harness.app.app()));

        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert_eq!(
            panel.origin.x, ANCHOR.origin.x,
            "左对齐锚点（BOTTOM_START）"
        );
        assert!(panel.origin.y >= ANCHOR.max().y, "面板在锚点下方");
        assert_eq!(
            panel.size.width.value(),
            200.0,
            "面板宽度 = MenuOptions.width"
        );
    }

    #[test]
    fn the_first_visible_frame_already_has_the_settled_rect() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        assert!(harness.open(&menu));
        let root = harness.root_entry(&menu);
        let first = root.hit_rect;
        assert_eq!(root.state, PopupState::Open, "首帧即 Open，无需等 reveal");
        harness.settle(&menu);
        let stable = harness.root_entry(&menu).hit_rect;
        assert_eq!(first, stable, "首可见帧 rect 必须等于稳定帧");
    }

    #[test]
    fn the_first_visible_frame_already_has_the_row_contents() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        assert!(harness.open(&menu));
        assert_eq!(menu.row_count(), 5, "行模型在打开时就绪（含分组 / 分隔线）");
        assert!(
            harness.find("menu:panel:list:row:1").is_some(),
            "首帧行节点已挂载"
        );
        assert!(
            harness.text_exists("资料"),
            "首帧行文本已经排版进树（C2：内容尺寸不晚于首帧）"
        );
        assert!(
            harness.find("menu:panel:list:row:1:content").is_some(),
            "行内容已填充"
        );
    }

    #[test]
    fn a_menu_near_the_bottom_flips_above_its_anchor() {
        let mut harness = Harness::new();
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let bottom = NodeBuilder::boxed()
            .id("bottom-anchor")
            .layout_override(LogicalRect::new(px(300.0), px(560.0), px(120.0), px(32.0)))
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(harness.window)
                    .expect("窗口")
                    .tree_mut(),
                root,
            )
            .expect("锚点");
        let menu = harness.menu(entries(), options());
        let anchor = Anchor::Node { node: bottom };
        let opened = menu
            .open(harness.app.app_mut(), harness.window, anchor)
            .expect("打开");
        assert!(opened);
        harness.settle(&menu);
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.origin.y.value() < 560.0 && panel.max().y.value() <= 592.0 + 0.5,
            "放不下时翻到锚点边沿上方（引擎 edge 锚点语义，不再按节点外框）：{panel:?}"
        );
    }

    #[test]
    fn arrow_keys_move_the_highlight_and_skip_non_actionable_rows() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1), "首个可激活行");

        harness.key(&menu, KeyCode::ArrowDown);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(4),
            "跳过禁用行与分隔线"
        );
        harness.key(&menu, KeyCode::ArrowDown);
        assert_eq!(menu.highlighted(harness.app.app()), Some(4), "末行不环绕");
        harness.key(&menu, KeyCode::ArrowUp);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
    }

    #[test]
    fn home_and_end_jump_to_the_first_and_last_actionable_rows() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::End);
        assert_eq!(menu.highlighted(harness.app.app()), Some(4));
        harness.key(&menu, KeyCode::Home);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
    }

    #[test]
    fn enter_activates_the_highlighted_entry_and_closes_the_menu() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::End);
        harness.key(&menu, KeyCode::Enter);

        assert_eq!(
            harness.activated.borrow().as_slice(),
            &[SmolStr::new_static("delete")],
            "回车激活高亮条目"
        );
        assert!(!menu.is_open(harness.app.app()), "选中后关闭菜单");
        harness.settle(&menu);
        assert_eq!(harness.closes.get(), 1, "关闭回调一次");
    }

    #[test]
    fn a_space_press_activates_the_highlighted_entry_like_enter() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::End);
        harness.key(&menu, KeyCode::Space);

        assert_eq!(
            harness.activated.borrow().as_slice(),
            &[SmolStr::new_static("delete")],
            "Space 与 Enter 走同一条激活路径"
        );
        harness.settle(&menu);
        assert_eq!(harness.closes.get(), 1, "激活后关闭一次");
        assert!(!menu.is_open(harness.app.app()));
    }

    #[test]
    fn an_a11y_click_activates_the_menu_entry_it_targets() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::End);
        assert_eq!(menu.highlighted(harness.app.app()), Some(4));

        let row = harness
            .find("menu:panel:list:row:1")
            .expect("首条可激活行节点");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(row).expect("行的 a11y 记录");
        assert_eq!(record.role, A11yRole::MenuItem);
        assert!(
            record.actions.contains(&A11yAction::Click),
            "MenuItem 必须宣告 a11y Click：{:?}",
            record.actions
        );

        let action = Action::Custom(SmolStr::new_static("menu:panel.row"));
        let handled = harness.app.app_mut().dispatch_action(
            harness.window,
            row,
            action,
            ActionSource::Programmatic,
        );
        assert!(
            handled,
            "AT Click 必须被分发到行 action（与引擎 on_click 同路径）"
        );
        harness.drain(&menu);
        assert_eq!(
            harness.activated.borrow().as_slice(),
            &[SmolStr::new_static("profile")],
            "AT Click 激活它指向的行，而不是当前高亮行"
        );
        harness.settle(&menu);
        assert_eq!(harness.closes.get(), 1);
        assert!(!menu.is_open(harness.app.app()));
    }

    #[test]
    fn group_and_separator_rows_expose_their_a11y_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let group = harness.find("menu:panel:list:row:0").expect("分组头行节点");
        let separator = harness.find("menu:panel:list:row:3").expect("分隔线行节点");
        let action = harness
            .find("menu:panel:list:row:1")
            .expect("普通菜单项行节点");
        let snapshot = full_a11y(&mut harness.app);
        let group_record = snapshot.node_for(group).expect("分组头 a11y");
        assert_eq!(
            group_record.role,
            A11yRole::Group,
            "菜单分组头按 Group 投影（D-P2-10 修正）"
        );
        assert_eq!(group_record.label_text(), Some("账户"));
        assert!(
            !group_record.actions.contains(&A11yAction::Focus),
            "分组头不可聚焦：{:?}",
            group_record.actions
        );
        let separator_record = snapshot.node_for(separator).expect("分隔线 a11y");
        assert_eq!(separator_record.role, A11yRole::Separator);
        assert!(separator_record.hidden, "菜单分隔线 a11y hidden（D-P2-10）");
        assert!(
            !separator_record.actions.contains(&A11yAction::Focus),
            "分隔线不可聚焦：{:?}",
            separator_record.actions
        );
        let action_record = snapshot.node_for(action).expect("菜单项 a11y");
        assert_eq!(action_record.role, A11yRole::MenuItem);
        assert!(!action_record.hidden);
        assert_eq!(action_record.label_text(), Some("资料"));
    }

    #[test]
    fn the_list_activation_path_activates_the_highlighted_entry() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::End);
        let viewport = menu
            .inner
            .panel
            .list()
            .viewport_node()
            .expect("列表 viewport");
        let action = Action::Custom(SmolStr::new_static("menu:panel:list.select"));
        let handled = harness.app.app_mut().dispatch_action(
            harness.window,
            viewport,
            action,
            ActionSource::Programmatic,
        );
        assert!(handled, "列表级激活路径必须被分发");
        harness.drain(&menu);
        assert_eq!(
            harness.activated.borrow().as_slice(),
            &[SmolStr::new_static("delete")],
            "列表 activate 回调与键盘/AT 共享 ItemPanel 激活路径"
        );
    }

    #[test]
    fn a_pointer_press_activates_the_entry_it_lands_on() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let at = harness.row_center(1);
        harness.click(&menu, at);

        assert_eq!(
            harness.activated.borrow().as_slice(),
            &[SmolStr::new_static("profile")],
            "点击行激活该条目"
        );
        assert!(!menu.is_open(harness.app.app()));
    }

    #[test]
    fn a_disabled_entry_is_not_activated() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let at = harness.row_center(2);
        harness.click(&menu, at);
        assert!(harness.activated.borrow().is_empty(), "禁用条目不得激活");
        assert!(menu.is_open(harness.app.app()), "禁用条目不关菜单");
    }

    #[test]
    fn escape_closes_the_menu_and_reports_it() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()), "Esc 关闭最上层浮层");
        harness.settle(&menu);
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn a_press_outside_closes_the_menu_and_replays_the_press_to_the_page() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.click(&menu, LogicalPoint::new(px(640.0), px(520.0)));
        assert!(!menu.is_open(harness.app.app()), "点击外部关闭");
        harness.settle(&menu);
        assert_eq!(harness.closes.get(), 1);
        assert_eq!(
            harness.background_clicks.get(),
            1,
            "D14：外部按下按 Replay 重派给下层命中控件"
        );
    }

    #[test]
    fn a_submenu_opens_beside_its_row_after_the_hover_delay() {
        let mut harness = Harness::new();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        let root = harness.root_entry(&menu).id;

        let at = harness.row_center(1);
        harness.hover(&menu, at);
        harness.advance(&menu, Duration::from_millis(150));
        assert_eq!(harness.stack().len(), 1, "200ms 延迟未到不开子菜单");

        harness.advance(&menu, Duration::from_millis(60));
        let entries = harness.stack();
        assert_eq!(entries.len(), 2, "延迟到点打开子菜单");
        let submenu = harness.submenu_entry(root);
        assert!(submenu.z > harness.root_entry(&menu).z, "子菜单 z 在父之上");
        let row_bounds = harness.bounds(harness.row(1));
        assert!(
            submenu.hit_rect.origin.x >= row_bounds.max().x - 0.5,
            "子菜单在所属行右侧：行 {row_bounds:?} / 子 {:?}",
            submenu.hit_rect
        );
        assert!(
            (submenu.hit_rect.origin.y - row_bounds.origin.y)
                .value()
                .abs()
                < 1.0,
            "子菜单与所属行顶部对齐"
        );
        assert!(
            harness.find("menu:sub:1:panel:list:row:0").is_some(),
            "子菜单行已挂载"
        );
    }

    #[test]
    fn a_submenu_row_keeps_static_haspopup_and_projects_expanded() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);

        let row = menu
            .inner
            .panel
            .list()
            .row_node(harness.app.app(), 1)
            .expect("子菜单行");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(row).expect("行 a11y");
        assert_eq!(
            record.has_popup,
            Some(HasPopup::Menu),
            "子菜单行必须持久宣告 aria-haspopup=menu"
        );
        assert_eq!(
            record.expanded,
            Some(true),
            "子菜单打开时父 menuitem 投影 expanded=true"
        );

        let submenu = menu.inner.submenus.borrow()[0].menu.clone();
        assert!(submenu.close(harness.app.app_mut()));
        harness.settle(&menu);
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(row).expect("行 a11y");
        assert_eq!(
            record.has_popup,
            Some(HasPopup::Menu),
            "关闭后静态 haspopup 不丢失"
        );
        assert_eq!(record.expanded, Some(false), "关闭后 expanded 复原为 false");
    }

    #[test]
    fn a_two_level_submenu_chain_attaches_to_its_parent_menu_items() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(nested_submenu_entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        let level2 = menu.inner.submenus.borrow()[0].menu.clone();
        harness.key(&level2, KeyCode::ArrowDown);
        harness.key(&level2, KeyCode::ArrowRight);
        let level3 = level2.inner.submenus.borrow()[0].menu.clone();

        let snapshot = full_a11y(&mut harness.app);
        let root_row = menu
            .inner
            .panel
            .list()
            .row_node(harness.app.app(), 1)
            .expect("root 子菜单行");
        let level2_row = level2
            .inner
            .panel
            .list()
            .row_node(harness.app.app(), 1)
            .expect("level2 子菜单行");
        let level2_popup = level2.popup_node().expect("level2 浮层");
        let level3_popup = level3.popup_node().expect("level3 浮层");
        let root_id = snapshot.id_for_engine(root_row).expect("root 行 a11y id");
        let level2_id = snapshot
            .id_for_engine(level2_row)
            .expect("level2 行 a11y id");
        assert_eq!(
            snapshot.node_for(level2_popup).expect("level2 a11y").parent,
            Some(root_id),
            "二级子菜单的 a11y 父必须是 root 的父 menuitem"
        );
        assert_eq!(
            snapshot.node_for(level3_popup).expect("level3 a11y").parent,
            Some(level2_id),
            "三级子菜单的 a11y 父必须是二级的父 menuitem"
        );
        let root_record = snapshot.node_for(root_row).expect("root 行 a11y");
        assert_eq!(root_record.has_popup, Some(HasPopup::Menu));
        assert_eq!(root_record.expanded, Some(true));
    }

    #[test]
    fn a_hover_opened_submenu_does_not_steal_focus() {
        let mut harness = Harness::new();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        let root = harness.root_entry(&menu).id;
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:panel:surface"),
            "根菜单打开时取焦到面板 surface"
        );

        let at = harness.row_center(1);
        harness.hover(&menu, at);
        harness.advance(&menu, Duration::from_millis(220));
        let _ = harness.submenu_entry(root);
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:panel:surface"),
            "D13：hover 打开子菜单不夺焦"
        );

        harness.key(&menu, KeyCode::Escape);
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:sub:1:panel:surface"),
            "键盘打开子菜单才把焦点移入"
        );
    }

    #[test]
    fn escape_closes_the_submenu_before_the_menu_it_belongs_to() {
        let mut harness = Harness::new();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        let root = harness.root_entry(&menu).id;
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(harness.stack().len(), 2, "→ 打开子菜单");

        harness.key(&menu, KeyCode::Escape);
        assert!(menu.is_open(harness.app.app()), "第一下 Esc 只关子菜单");
        let submenu = harness.submenu_entry(root);
        assert!(
            !submenu.state.is_dismissable(),
            "子菜单进入关闭事务（不可再命中）"
        );

        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()), "第二下 Esc 关父菜单");
        harness.settle(&menu);
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn a_trigger_press_during_the_close_transaction_reopens_the_menu() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()));
        harness.click(&menu, LogicalPoint::new(px(310.0), px(210.0)));
        assert!(
            menu.is_open(harness.app.app()),
            "Closing 中按 opener 由引擎取消关闭（Toggle 消费，不再二次打开）"
        );
    }

    #[test]
    fn a_long_menu_virtualizes_its_rows_and_scrolls_to_the_end() {
        let mut harness = Harness::new();
        let entries: Vec<MenuEntry> = (0..80)
            .map(|index| {
                let id = format!("entry-{index}");
                MenuEntry::Action(MenuAction::new(id.clone(), id))
            })
            .collect();
        let menu = harness.menu(
            entries,
            MenuOptions {
                max_height: px(200.0),
                ..options()
            },
        );
        harness.open(&menu);
        assert_eq!(menu.row_count(), 80);
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.size.height.value() <= 200.0 + 0.5,
            "面板高度被 max_height 截住：{panel:?}"
        );
        let mounted = count_rows(&harness);
        assert!(
            mounted < 20,
            "长菜单只挂可见窗口内的行（实际 {mounted} / 80）"
        );

        harness.key(&menu, KeyCode::End);
        assert_eq!(menu.highlighted(harness.app.app()), Some(79));
        assert!(
            harness.find("menu:panel:list:row:79").is_some(),
            "末行已挂载"
        );
    }

    #[test]
    fn a_small_viewport_shrinks_the_menu_and_scrolls_its_last_row_into_view() {
        let mut harness = Harness::sized(LogicalSize::new(px(260.0), px(200.0)));
        let anchor = {
            let root = harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .root();
            NodeBuilder::boxed()
                .id("small-anchor")
                .layout_override(LogicalRect::new(px(10.0), px(10.0), px(80.0), px(24.0)))
                .build(
                    harness
                        .app
                        .app_mut()
                        .window_mut(harness.window)
                        .expect("窗口")
                        .tree_mut(),
                    root,
                )
                .expect("锚点")
        };
        let entries: Vec<MenuEntry> = (0..12)
            .map(|index| {
                let id = format!("small-{index}");
                MenuEntry::Action(MenuAction::new(id.clone(), id))
            })
            .collect();
        let menu = harness.menu(entries, options());
        let opened = menu
            .open(
                harness.app.app_mut(),
                harness.window,
                Anchor::Node { node: anchor },
            )
            .expect("打开");
        assert!(opened);
        harness.settle(&menu);

        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.max().y <= px(196.5),
            "面板必须被视口夹住并留在边距内：{panel:?}"
        );
        assert!(
            panel.size.height < px(320.0),
            "小视口下面板必须收缩到可用高度（内部滚动而不是整块溢出）：{panel:?}"
        );

        harness.key(&menu, KeyCode::End);
        harness.settle(&menu);
        assert_eq!(menu.highlighted(harness.app.app()), Some(11));
        let last = harness.screen_bounds(harness.row(11));
        assert!(
            last.origin.y >= panel.origin.y - 0.5 && last.max().y <= panel.max().y + 0.5,
            "End 后末行必须滚进面板可见区：row={last:?} panel={panel:?}"
        );
    }

    #[test]
    fn an_ancestor_scroll_closes_the_whole_menu_chain() {
        let mut harness = Harness::sized(LogicalSize::new(px(800.0), px(600.0)));
        let page = {
            let root = harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .root();
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("scroll-page")
                .style(
                    StyleBuilder::new()
                        .relative()
                        .width(px(800.0))
                        .height(px(300.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, root)
                .expect("滚动页面")
        };
        let anchor = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("scroll-anchor")
                .layout_override(LogicalRect::new(px(20.0), px(20.0), px(120.0), px(32.0)))
                .build(tree, page)
                .expect("滚动锚点")
        };
        let scroll = harness
            .app
            .app_mut()
            .make_scroll(harness.window, page, ScrollConfig::default())
            .expect("滚动容器");
        harness
            .app
            .app_mut()
            .set_scroll_content(&scroll, LogicalSize::new(px(800.0), px(2400.0)))
            .expect("滚动内容");
        let menu = harness.menu(entries(), options());
        assert!(
            menu.open(
                harness.app.app_mut(),
                harness.window,
                Anchor::Node { node: anchor },
            )
            .expect("打开")
        );
        harness.settle(&menu);
        assert!(menu.is_open(harness.app.app()));

        harness
            .app
            .app_mut()
            .scroll_to(&scroll, LogicalPoint::new(px(0.0), px(10.0)))
            .expect("滚动");
        harness.settle(&menu);
        assert!(
            !menu.is_open(harness.app.app()),
            "菜单的祖先滚动必须关闭整链（设计 §4.7 矩阵）"
        );
    }

    #[test]
    fn a_window_resize_closes_the_menu_chain() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        assert!(harness.open(&menu));
        harness
            .app
            .app_mut()
            .window_mut(harness.window)
            .expect("窗口")
            .set_size(LogicalSize::new(px(700.0), px(500.0)));
        harness.settle(&menu);
        assert!(
            !menu.is_open(harness.app.app()),
            "窗口 resize 必须关闭菜单整链（设计 §4.7 矩阵）"
        );
    }

    #[test]
    fn the_menu_declares_menu_and_menu_item_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let snapshot = full_a11y(&mut harness.app);

        let panel = menu.panel_node().expect("面板");
        let wrapper = menu.popup_node().expect("浮层内容节点");
        let wrapper_record = snapshot.node_for(wrapper).expect("包装节点在 a11y 树里");
        assert_eq!(wrapper_record.role, A11yRole::Menu);
        assert_eq!(
            snapshot.node_for(panel).expect("面板在 a11y 树里").role,
            A11yRole::GenericContainer,
            "面板节点不再重复声明 Menu"
        );

        let row = {
            let mut found = None;
            for node in snapshot.nodes() {
                if node.role == A11yRole::MenuItem {
                    found = Some(node);
                    break;
                }
            }
            found.expect("行声明 MenuItem")
        };
        let label = row
            .label
            .as_ref()
            .map(|label| label.text().to_owned())
            .unwrap_or_default();
        assert_eq!(label, "资料", "MenuItem 的可访问名是条目文本");

        let row_for_blocked = snapshot
            .nodes()
            .iter()
            .find(|node| {
                node.role == A11yRole::MenuItem
                    && node
                        .label
                        .as_ref()
                        .is_some_and(|label| label.text() == "已禁用")
            })
            .expect("禁用条目也在 a11y 树里");
        assert!(row_for_blocked.disabled, "禁用态声明到 a11y");
    }

    #[test]
    fn releasing_a_menu_does_not_report_a_close() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        assert!(harness.open(&menu));
        let _ = menu.release(harness.app.app_mut());
        assert_eq!(
            harness.closes.get(),
            0,
            "release 是静默拆卸，不得触发 on_close（K1-13）"
        );
        assert!(!menu.is_open(harness.app.app()));
    }

    #[test]
    fn a_settled_menu_with_no_input_requests_no_frames() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.settle(&menu);
        assert!(harness.idles(&menu), "稳态不出帧（I3）");
    }

    #[test]
    fn a_menu_is_non_modal_and_keeps_background_hover() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        assert!(
            harness.app.app().modal_scope(harness.window).is_none(),
            "菜单是 Auto 非模态（不再是旧 modal/trap）"
        );
        let background = harness.background;
        let _ = harness.app.inject(InputEvent::pointer_move(
            LogicalPoint::new(px(620.0), px(520.0)),
            Modifiers::empty(),
        ));
        let _ = harness.app.drain().expect("帧");
        assert_eq!(
            harness.app.app().hovered(harness.window),
            Some(background),
            "非模态菜单不阻断背景 hover"
        );
    }

    #[test]
    fn opening_and_closing_the_menu_never_forces_a_full_repaint() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        assert!(harness.open(&menu));
        for _ in 0..4 {
            let record = harness.drain(&menu);
            assert!(!record.damage_full, "打开菜单不得全量重绘");
        }
        let id = harness.root_entry(&menu).id;
        assert!(menu.close(harness.app.app_mut()));
        for _ in 0..12 {
            harness.app.advance(Duration::from_millis(16));
            let record = harness.drain(&menu);
            assert!(!record.damage_full, "关闭菜单不得全量重绘");
            if harness.app.app().popup_rect(id).is_none() {
                break;
            }
        }
        assert!(
            harness.app.app().popup_rect(id).is_none(),
            "关闭动画结束后节点卸载"
        );
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn the_item_panel_debug_ids_stay_stable() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let panel = menu.panel_node().expect("面板");
        assert_eq!(
            harness.find("menu:panel:panel"),
            Some(panel),
            "PopupPanel 面板节点保持 {{key}}:panel 命名"
        );
        assert!(
            harness.find("menu:panel:surface").is_some(),
            "surface 保持 {{key}}:surface 命名"
        );
        assert!(
            harness.find("menu:panel:list:viewport").is_some(),
            "列表 viewport 保持 {{key}}:list:viewport 命名（menu_metrics 依赖）"
        );
    }

    fn count_rows(harness: &Harness) -> usize {
        let tree = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree();
        let mut stack = vec![tree.root()];
        let mut rows = 0;
        while let Some(node) = stack.pop() {
            if let Some(id) = tree.debug_id(node)
                && id.starts_with("menu:panel:list:row:")
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

    fn sibling_entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("first", "第一")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "甲",
                [MenuEntry::Action(MenuAction::new("a", "A"))],
            )),
            MenuEntry::Submenu(MenuSubmenu::new(
                "乙",
                [MenuEntry::Action(MenuAction::new("b", "B"))],
            )),
        ]
    }

    fn three_level_entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("a", "A")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [
                    MenuEntry::Action(MenuAction::new("inner", "内部")),
                    MenuEntry::Submenu(MenuSubmenu::new(
                        "更深",
                        [MenuEntry::Action(MenuAction::new("deep", "深处"))],
                    )),
                ],
            )),
        ]
    }

    fn open_three_levels(harness: &mut Harness, menu: &Menu) {
        harness.open(menu);
        harness.hover_row_of(menu, "menu:panel", 1);
        harness.advance(menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2, "hover 打开二级");
        let level2 = harness
            .find("menu:sub:1:panel:list:row:1")
            .expect("二级子菜单行");
        let bounds = harness.screen_bounds(level2);
        let at = LogicalPoint::new(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        harness.hover_point(menu, at);
        harness.advance(menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 3, "hover 打开三级");
    }

    #[test]
    fn a_three_level_hover_cascade_stacks_newest_on_top() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        open_three_levels(&mut harness, &menu);
        let entries = harness.stack();
        let ids: Vec<PopupId> = entries.iter().map(|entry| entry.id).collect();
        assert_eq!(ids.len(), 3);
        assert!(entries[0].z < entries[1].z && entries[1].z < entries[2].z);
        assert_eq!(entries[1].parent, Some(entries[0].id), "二级挂在根下");
        assert_eq!(entries[2].parent, Some(entries[1].id), "三级挂在二级下");
        assert!(harness.find("menu:sub:1:sub:1:panel:list:row:0").is_some());
        assert!(menu.is_open(harness.app.app()), "三级链的根仍打开");
    }

    #[test]
    fn pointer_through_the_safe_triangle_keeps_the_child_and_ignores_siblings() {
        let mut harness = Harness::new();
        let menu = harness.menu(sibling_entries(), options());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        let root = harness.root_entry(&menu).id;
        let child = harness.submenu_entry(root);
        assert_eq!(child.parent, Some(root));

        let child_rect = child.hit_rect;
        let row2 = harness.screen_bounds(harness.row(2));
        let at = LogicalPoint::new(child_rect.origin.x - 2.0, row2.origin.y + 2.0);
        harness.hover_point(&menu, at);
        harness.advance(&menu, Duration::from_millis(400));
        assert_eq!(harness.stack().len(), 2, "安全三角内不得替换兄弟");
        let still = harness.submenu_entry(root);
        assert_eq!(still.id, child.id);
        assert!(
            menu.highlighted(harness.app.app()) == Some(1),
            "grace 期间保持父项高亮"
        );
    }

    #[test]
    fn a_pointer_outside_the_triangle_replaces_the_open_sibling() {
        let mut harness = Harness::new();
        let menu = harness.menu(sibling_entries(), options());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        let root = harness.root_entry(&menu).id;
        let first = harness.submenu_entry(root);

        let panel = harness.bounds(menu.panel_node().expect("面板"));
        let row2 = harness.screen_bounds(harness.row(2));
        let at = LogicalPoint::new(panel.origin.x + 8.0, row2.origin.y + row2.size.height / 2.0);
        harness.hover_point(&menu, at);
        assert!(harness.submenu_entry_opt(root).is_none(), "旧子级立即关闭");
        assert_eq!(harness.stack().len(), 1);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2, "新兄弟行在延迟后打开");
        let second = harness.submenu_entry(root);
        assert_ne!(second.id, first.id);
        assert_eq!(
            second.hit_rect.origin.y,
            harness.screen_bounds(harness.row(2)).origin.y
        );
    }

    #[test]
    fn leaving_the_chain_closes_the_pointer_opened_levels_after_the_grace() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        open_three_levels(&mut harness, &menu);
        let root = harness.root_entry(&menu).id;

        harness.hover_point(&menu, LogicalPoint::new(px(700.0), px(520.0)));
        harness.advance(&menu, Duration::from_millis(200));
        assert_eq!(harness.stack().len(), 3, "300ms 宽限内保持链");
        harness.advance(&menu, Duration::from_millis(150));
        assert_eq!(harness.stack().len(), 1, "宽限过后只关闭 hover 打开的层");
        assert!(menu.is_open(harness.app.app()), "click-opened 根保留");
        assert!(
            harness.submenu_entry_opt(root).is_none(),
            "pointer-opened 子级确已关闭"
        );
    }

    #[test]
    fn reentering_the_chain_during_the_grace_cancels_the_close() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        open_three_levels(&mut harness, &menu);
        let root = harness.root_entry(&menu).id;

        harness.hover_point(&menu, LogicalPoint::new(px(700.0), px(520.0)));
        harness.advance(&menu, Duration::from_millis(150));
        let at = harness.center_of("menu:sub:1:panel:list:row:1");
        harness.hover_point(&menu, at);
        harness.advance(&menu, Duration::from_millis(400));
        assert_eq!(harness.stack().len(), 3, "grace 内回到链取消关闭");
        assert!(harness.submenu_entry(root).state.is_dismissable());
    }

    #[test]
    fn a_press_on_an_ancestor_level_closes_only_its_descendants() {
        let mut harness = Harness::new();
        let menu = harness.menu(
            vec![
                MenuEntry::Group(SmolStr::new_static("分组")),
                MenuEntry::Submenu(MenuSubmenu::new(
                    "更多",
                    [
                        MenuEntry::Group(SmolStr::new_static("内部")),
                        MenuEntry::Submenu(MenuSubmenu::new(
                            "更深",
                            [MenuEntry::Action(MenuAction::new("deep", "深处"))],
                        )),
                    ],
                )),
            ],
            options(),
        );
        open_three_levels(&mut harness, &menu);

        let group = harness.center_of("menu:panel:list:row:0");
        harness.click(&menu, group);
        harness.settle(&menu);
        assert!(menu.is_open(harness.app.app()), "根层保留");
        assert_eq!(harness.stack().len(), 1, "只关闭后代层");
        assert!(harness.find("menu:sub:1:panel:list:row:0").is_none());
    }

    #[test]
    fn escape_closes_one_level_at_a_time_children_first() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        open_three_levels(&mut harness, &menu);
        let root = harness.root_entry(&menu).id;
        assert_eq!(harness.stack().len(), 3);

        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);

        assert_eq!(harness.stack().len(), 2, "第一下 Esc 只关三级");
        assert!(harness.submenu_entry(root).state.is_dismissable());
        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        assert_eq!(harness.stack().len(), 1, "第二下关二级");
        assert!(menu.is_open(harness.app.app()), "根仍打开");
        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        assert!(!menu.is_open(harness.app.app()), "第三下关根");
        assert_eq!(harness.closes.get(), 1);
    }

    fn long_menu_entries(count: usize) -> Vec<MenuEntry> {
        (0..count)
            .map(|index| {
                MenuEntry::Action(MenuAction::new(
                    format_smolstr!("entry-{index}"),
                    format_smolstr!("条目 {index}"),
                ))
            })
            .collect()
    }

    #[test]
    fn the_keyboard_table_pages_and_jumps_inside_a_long_menu() {
        let mut harness = Harness::new();
        let menu = harness.menu(
            long_menu_entries(40),
            MenuOptions {
                max_height: px(200.0),
                ..options()
            },
        );
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        assert_eq!(menu.highlighted(harness.app.app()), Some(0));
        harness.key(&menu, KeyCode::PageDown);
        let page = menu
            .highlighted(harness.app.app())
            .expect("PageDown 后高亮");
        assert!(page >= 2, "PageDown 至少前进 2 行（实际 {page}）");
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        let row = harness.bounds(
            harness
                .find(&format!("menu:panel:list:row:{page}"))
                .expect("页内行"),
        );
        assert!(
            row.origin.y >= panel.origin.y - 0.5 && row.origin.y < panel.max().y,
            "PageDown 后的高亮行进入面板可视区：row={row:?} panel={panel:?}"
        );
        harness.key(&menu, KeyCode::PageUp);
        assert_eq!(menu.highlighted(harness.app.app()), Some(0));
        harness.key(&menu, KeyCode::End);
        assert_eq!(menu.highlighted(harness.app.app()), Some(39));
        assert!(
            harness.find("menu:panel:list:row:39").is_some(),
            "End 滚到末行"
        );
        harness.key(&menu, KeyCode::Home);
        assert_eq!(menu.highlighted(harness.app.app()), Some(0));
    }

    #[test]
    fn right_opens_the_submenu_with_focus_and_left_returns_to_the_parent_row() {
        let mut harness = Harness::new();
        let menu = harness.menu(sibling_entries(), options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::ArrowDown);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1), "移到子菜单行");
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(harness.stack().len(), 2, "→ 打开子菜单");
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:sub:1:panel:surface"),
            "键盘打开子菜单把焦点移入"
        );
        assert_eq!(
            harness
                .app
                .app()
                .popup_stack(harness.window)
                .expect("栈")
                .entries
                .len(),
            2
        );
        harness.key(&menu, KeyCode::ArrowLeft);
        assert_eq!(harness.stack().len(), 1, "← 关闭当前子菜单");
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:panel:surface"),
            "← 后焦点回到父层"
        );
        assert_eq!(menu.highlighted(harness.app.app()), Some(1), "← 后高亮父项");
    }

    fn focusable_node(harness: &mut Harness, id: &'static str, rect: LogicalRect) -> NodeId {
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let action = Action::Custom(format_smolstr!("{id}.click"));
        harness.app.app_mut().on_action(action.clone(), move |_| {});
        NodeBuilder::boxed()
            .id(id)
            .tab_stop(true)
            .on_click(action)
            .layout_override(rect)
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(harness.window)
                    .expect("窗口")
                    .tree_mut(),
                root,
            )
            .expect("可聚焦节点")
    }

    #[test]
    fn tab_closes_the_chain_and_moves_focus_past_the_opener() {
        let mut harness = Harness::new();
        let opener = focusable_node(
            &mut harness,
            "tab-opener",
            LogicalRect::new(px(200.0), px(40.0), px(80.0), px(32.0)),
        );
        let after = focusable_node(
            &mut harness,
            "tab-after",
            LogicalRect::new(px(600.0), px(40.0), px(80.0), px(32.0)),
        );
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(opener));
        let menu = harness.menu(entries(), options());
        menu.open(
            harness.app.app_mut(),
            harness.window,
            Anchor::Node { node: opener },
        )
        .expect("打开");
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key_with(&menu, KeyCode::Tab, Modifiers::empty());
        assert!(!menu.is_open(harness.app.app()), "Tab 关闭整链");
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(after),
            "焦点移到 opener 之后的下一个 tab stop"
        );
    }

    fn typeahead_entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("alpha", "Alpha")),
            MenuEntry::Action(MenuAction::new("beta", "Beta").disabled(true)),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::new("gamma", "Gamma")),
            MenuEntry::Action(MenuAction::new("golf", "Golf")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "Delta",
                [MenuEntry::Action(MenuAction::new("d", "D"))],
            )),
        ]
    }

    #[test]
    fn typeahead_cycles_matches_skips_disabled_and_appends_characters() {
        let mut harness = Harness::new();
        let menu = harness.menu(typeahead_entries(), options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));

        harness.key(&menu, KeyCode::KeyG);
        assert_eq!(menu.highlighted(harness.app.app()), Some(3), "G 匹配 Gamma");
        harness.key(&menu, KeyCode::KeyG);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(4),
            "重复 G 循环到 Golf"
        );
        harness.key(&menu, KeyCode::KeyG);
        assert_eq!(menu.highlighted(harness.app.app()), Some(3), "循环回 Gamma");
        harness.key(&menu, KeyCode::KeyB);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(3),
            "禁用项 Beta 被跳过，高亮不变"
        );
        harness.advance(&menu, Duration::from_millis(2100));
        harness.key(&menu, KeyCode::KeyD);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(5),
            "子菜单标题也参与 typeahead"
        );
    }

    #[test]
    fn typeahead_appends_characters_and_expires_after_the_timeout() {
        let mut harness = Harness::new();
        let menu = harness.menu(typeahead_entries(), options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::End);
        assert_eq!(menu.highlighted(harness.app.app()), Some(5), "先移到末行");
        harness.key(&menu, KeyCode::KeyA);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(0),
            "A 从末行跳回 Alpha"
        );
        harness.key(&menu, KeyCode::KeyL);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(0),
            "Al 继续匹配 Alpha"
        );
        harness.advance(&menu, Duration::from_millis(2100));
        assert!(
            menu.inner.sink.chain().typeahead_buffer().is_empty(),
            "2s 超时清空缓冲"
        );
        harness.key(&menu, KeyCode::KeyG);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(3),
            "超时后缓冲清空，G 匹配 Gamma"
        );
        harness.key(&menu, KeyCode::KeyB);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(3),
            "禁用项 Beta 是唯一 B 候选，被跳过后高亮不变"
        );
        harness.advance(&menu, Duration::from_millis(2100));
        harness.key(&menu, KeyCode::KeyD);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(5),
            "缓冲清空后 D 匹配子菜单 Delta"
        );
    }

    #[test]
    fn a_text_event_never_feeds_the_typeahead_buffer() {
        let mut harness = Harness::new();
        let menu = harness.menu(typeahead_entries(), options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        let _ = harness.app.inject(InputEvent::Text {
            text: SmolStr::new_static("g"),
            timestamp: std::time::Instant::now(),
        });
        harness.drain(&menu);
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(0),
            "IME 文本事件不进入 typeahead（只认按键）"
        );
    }

    fn checkable_entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("show", "显示").checkbox(false)),
            MenuEntry::Action(MenuAction::new("r1", "选项一").radio(true)),
            MenuEntry::Action(MenuAction::new("r2", "选项二").radio(false)),
        ]
    }

    #[test]
    fn space_toggles_a_checkbox_without_closing_and_enter_selects_and_closes() {
        let mut harness = Harness::new();
        let toggles: Rc<RefCell<Vec<MenuActivation>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = toggles.clone();
        let menu = harness
            .menu(checkable_entries(), options())
            .on_activate(move |_, activation| sink.borrow_mut().push(activation));
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        assert_eq!(menu.highlighted(harness.app.app()), Some(0));

        harness.key(&menu, KeyCode::Space);
        assert!(menu.is_open(harness.app.app()), "Space 切换不关菜单");
        let recorded = toggles.borrow();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].check,
            Some(MenuCheckState {
                kind: MenuCheck::Checkbox,
                checked: true
            })
        );
        let entries = menu.entries();
        assert_eq!(
            entries.first(),
            Some(&MenuEntry::Action(
                MenuAction::new("show", "显示").checkbox(true)
            ))
        );

        drop(recorded);
        harness.key(&menu, KeyCode::Enter);
        assert!(!menu.is_open(harness.app.app()), "Enter 选择并关链");
        assert_eq!(toggles.borrow().len(), 2);
        assert_eq!(
            toggles.borrow()[1].check,
            Some(MenuCheckState {
                kind: MenuCheck::Checkbox,
                checked: false
            })
        );
    }

    #[test]
    fn radio_members_are_mutually_exclusive_and_expose_their_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(checkable_entries(), options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::End);
        assert_eq!(menu.highlighted(harness.app.app()), Some(2));
        harness.key(&menu, KeyCode::Space);
        assert!(menu.is_open(harness.app.app()));
        let entries = menu.entries();
        assert_eq!(
            entries.get(1),
            Some(&MenuEntry::Action(
                MenuAction::new("r1", "选项一").radio(false)
            )),
            "组内旧选项被清空"
        );
        assert_eq!(
            entries.get(2),
            Some(&MenuEntry::Action(
                MenuAction::new("r2", "选项二").radio(true)
            ))
        );

        let snapshot = full_a11y(&mut harness.app);
        let row = menu
            .inner
            .panel
            .list()
            .row_node(harness.app.app(), 2)
            .expect("radio 行");
        let record = snapshot.node_for(row).expect("radio a11y");
        assert_eq!(record.role, A11yRole::MenuItemRadio);
        assert_eq!(record.checked, Some(true));
    }

    #[test]
    fn the_active_item_is_projected_as_the_menu_active_descendant() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        let content = menu.popup_node().expect("popup content");
        let row1 = harness.row(1);
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(content).expect("content a11y");
        assert_eq!(record.role, A11yRole::Menu);
        assert_eq!(
            record.active_descendant,
            snapshot.id_for_engine(row1),
            "role=Menu 的 popup content 根 active_descendant 指向 active_item"
        );

        harness.key(&menu, KeyCode::End);
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(content).expect("content a11y");
        assert_eq!(
            record.active_descendant,
            snapshot.id_for_engine(harness.row(4)),
            "键盘移动后 active_descendant 跟随"
        );
        assert_eq!(
            snapshot
                .node_for(menu.inner.panel.surface_node().expect("surface"))
                .expect("surface")
                .role,
            A11yRole::GenericContainer
        );
    }

    #[test]
    fn the_active_descendant_clears_when_the_active_row_unmounts() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(
            long_menu_entries(40),
            MenuOptions {
                max_height: px(200.0),
                ..options()
            },
        );
        harness.open(&menu);
        let content = menu.popup_node().expect("popup content");
        let snapshot = full_a11y(&mut harness.app);
        assert!(
            snapshot
                .node_for(content)
                .expect("content a11y")
                .active_descendant
                .is_some(),
            "初始 active_descendant 存在"
        );
        menu.inner
            .panel
            .list()
            .scroll_by(harness.app.app_mut(), px(600.0))
            .expect("滚动菜单");
        harness.drain(&menu);
        let snapshot = full_a11y(&mut harness.app);
        assert_eq!(
            snapshot
                .node_for(content)
                .expect("content a11y")
                .active_descendant,
            None,
            "active_item 行卸载后 active_descendant 清空"
        );
    }

    #[test]
    fn alt_f10_on_the_trigger_opens_the_menu_with_the_first_row_active() {
        let mut harness = Harness::new();
        let trigger = focusable_node(
            &mut harness,
            "alt-f10-trigger",
            LogicalRect::new(px(200.0), px(40.0), px(80.0), px(32.0)),
        );
        let menu = harness.menu(entries(), options());
        menu.bind_trigger(harness.app.app_mut(), harness.window, trigger)
            .expect("绑定 Alt+F10");
        assert!(
            harness
                .app
                .app_mut()
                .set_focus(harness.window, Some(trigger)),
            "触发节点可聚焦"
        );
        harness.key_with(&menu, KeyCode::F10, Modifiers::ALT);
        assert!(menu.is_open(harness.app.app()), "Alt+F10 打开菜单");
        assert_eq!(menu.highlighted(harness.app.app()), Some(1), "首项 active");
    }

    #[test]
    fn a_hover_opened_child_keeps_its_own_active_item_while_the_pointer_moves() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        open_three_levels(&mut harness, &menu);
        let root = harness.root_entry(&menu).id;
        let level2_state = harness.submenu_entry(root);
        let level2_menu = menu.inner.submenus.borrow()[0].menu.clone();
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
        harness.hover_row_of(&menu, "menu:sub:1:panel", 0);
        assert_eq!(
            level2_menu.highlighted(harness.app.app()),
            Some(0),
            "hover 到二级 action 行后二级 active 为 0"
        );
        harness.hover_row_of(&menu, "menu:sub:1:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(
            level2_menu.highlighted(harness.app.app()),
            Some(1),
            "hover 到二级的子菜单行后二级 active 真的移动"
        );
        assert_eq!(
            menu.highlighted(harness.app.app()),
            Some(1),
            "根的 active 不被子层的 hover 改变"
        );
        assert!(
            level2_state.state.is_dismissable(),
            "二级保持打开（指针在其矩形内）"
        );
        assert_eq!(harness.stack().len(), 3, "三级由 hover 级联打开");
        harness.hover_row_of(&menu, "menu:sub:1:sub:1:panel", 0);
        let level3_menu = menu.inner.submenus.borrow()[0].menu.inner.submenus.borrow()[0]
            .menu
            .clone();
        assert_eq!(
            level3_menu.highlighted(harness.app.app()),
            Some(0),
            "三级的 active 跟随三级内部 hover"
        );
        assert_eq!(
            level2_menu.highlighted(harness.app.app()),
            Some(1),
            "二级 active 不被三级 hover 改变"
        );
    }

    #[test]
    fn the_anchor_matrix_closes_report_anchor_lost_and_programmatic_closes_report_programmatic() {
        let mut harness = Harness::new();
        let reasons: Rc<RefCell<Vec<DismissReason>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = reasons.clone();
        let menu = Menu::new(
            harness.app.app_mut(),
            harness.theme.clone(),
            "menu",
            entries(),
            options(),
        )
        .on_close(move |_, reason| sink.borrow_mut().push(reason));
        assert!(harness.open(&menu));
        harness
            .app
            .app_mut()
            .window_mut(harness.window)
            .expect("窗口")
            .set_size(LogicalSize::new(px(700.0), px(500.0)));
        harness.advance(&menu, Duration::from_millis(40));
        harness.settle(&menu);
        assert!(
            !menu.is_open(harness.app.app()),
            "窗口 resize 必须关闭整链（锚点矩阵）"
        );
        assert_eq!(
            reasons.borrow().as_slice(),
            &[DismissReason::AnchorLost],
            "锚点矩阵关闭必须上报 AnchorLost（R2-02/Q2-07）"
        );

        assert!(harness.open(&menu));
        assert!(menu.close(harness.app.app_mut()));
        harness.settle(&menu);
        assert_eq!(
            reasons.borrow().as_slice(),
            &[DismissReason::AnchorLost, DismissReason::Programmatic],
            "程序化关闭仍上报 Programmatic"
        );
    }

    #[test]
    fn the_menu_slides_in_without_moving_the_settled_rect() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        assert!(harness.open(&menu));
        let node = menu.popup_node().expect("浮层节点");
        let opening = harness.root_entry(&menu);
        assert_eq!(opening.node, node, "打开时菜单必须在栈里");
        assert!(
            opening.paint_offset.x == LogicalPx::ZERO
                && opening.paint_offset.y > LogicalPx::ZERO
                && opening.paint_offset.y <= px(4.0),
            "FadeSlide 首帧必须带向下的 4px 绘制偏移：{:?}（state={:?}）",
            opening.paint_offset,
            opening.state
        );
        let first_rect = harness.popup_rect(&menu);
        for _ in 0..4 {
            harness.app.advance(Duration::from_millis(16));
            let record = harness.drain(&menu);
            assert!(
                !record.damage_full,
                "slide 帧必须局部重绘（I6）：{:?}",
                record.stats
            );
        }
        harness.settle(&menu);
        let settled = harness.root_entry(&menu);
        assert_eq!(
            settled.paint_offset,
            LogicalPoint::ZERO,
            "动画结束后 paint_offset 必须归零"
        );
        assert_eq!(settled.state, PopupState::Open);
        assert_eq!(
            harness.popup_rect(&menu),
            first_rect,
            "slide 只改绘制偏移，settle rect 必须首帧即稳定"
        );

        assert!(menu.close(harness.app.app_mut()), "关闭动画开始");
        harness.app.advance(Duration::from_millis(16));
        let closing = harness.drain(&menu);
        assert!(
            !closing.damage_full,
            "关闭 slide 帧必须局部重绘：{:?}",
            closing.stats
        );
        let dpr = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .device_pixel_ratio();
        let expected = dpr.to_covering_device_rect(first_rect);
        let covered = closing.damage.iter().any(|damage| {
            damage.x0.value() <= expected.x0.value()
                && damage.y0.value() <= expected.y0.value()
                && damage.x1.value() >= expected.x1.value()
                && damage.y1.value() >= expected.y1.value()
        });
        assert!(
            covered,
            "关闭帧 damage 必须覆盖旧菜单 rect（无残影）：damage={:?} rect={expected:?}",
            closing.damage
        );
    }

    #[test]
    fn escape_returns_focus_to_the_parent_level() {
        let mut harness = Harness::new();
        let menu = harness.menu(sibling_entries(), options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:sub:1:panel:surface")
        );
        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        assert_eq!(harness.stack().len(), 1);
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:panel:surface"),
            "Esc 关闭子菜单后焦点逐层归还父层"
        );
        assert!(menu.is_open(harness.app.app()), "父层在子菜单关闭后仍打开");
    }

    #[test]
    fn the_close_callback_reports_the_engine_dismiss_reason() {
        let mut harness = Harness::new();
        let reasons: Rc<RefCell<Vec<DismissReason>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = reasons.clone();
        let menu = Menu::new(
            harness.app.app_mut(),
            harness.theme.clone(),
            "menu",
            entries(),
            options(),
        )
        .on_close(move |_, reason| sink.borrow_mut().push(reason));
        harness.open(&menu);
        harness.click(&menu, LogicalPoint::new(px(640.0), px(520.0)));
        harness.settle(&menu);
        assert_eq!(reasons.borrow().as_slice(), &[DismissReason::OutsidePress]);

        harness.open(&menu);
        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        assert_eq!(
            reasons.borrow().as_slice(),
            &[DismissReason::OutsidePress, DismissReason::Escape]
        );
    }

    #[test]
    fn a_three_level_a11y_chain_nests_and_projects_expanded() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(three_level_entries(), options());
        harness.open(&menu);
        let level2 = menu
            .open_submenu(harness.app.app_mut(), 1)
            .expect("二级")
            .expect("二级句柄");
        let level3 = level2
            .open_submenu(harness.app.app_mut(), 1)
            .expect("三级")
            .expect("三级句柄");
        harness.settle(&menu);
        let snapshot = full_a11y(&mut harness.app);
        let root_row = menu
            .inner
            .panel
            .list()
            .row_node(harness.app.app(), 1)
            .expect("root 子菜单行");
        let level2_row = level2
            .inner
            .panel
            .list()
            .row_node(harness.app.app(), 1)
            .expect("level2 子菜单行");
        let root_row_id = snapshot.id_for_engine(root_row).expect("root 行 id");
        let level2_row_id = snapshot.id_for_engine(level2_row).expect("level2 行 id");
        assert_eq!(
            snapshot
                .node_for(level2.popup_node().expect("二级浮层"))
                .expect("二级 a11y")
                .parent,
            Some(root_row_id),
            "二级挂到根的父 menuitem"
        );
        assert_eq!(
            snapshot
                .node_for(level3.popup_node().expect("三级浮层"))
                .expect("三级 a11y")
                .parent,
            Some(level2_row_id),
            "三级挂到二级的父 menuitem"
        );
        assert_eq!(
            snapshot.node_for(root_row).expect("root 行 a11y").expanded,
            Some(true)
        );
        assert_eq!(
            snapshot
                .node_for(level2_row)
                .expect("level2 行 a11y")
                .expanded,
            Some(true)
        );
    }

    #[test]
    fn scrolling_the_parent_row_out_of_view_closes_the_whole_chain() {
        let mut harness = Harness::new();
        let mut entries = long_menu_entries(80);
        entries.insert(
            2,
            MenuEntry::Submenu(MenuSubmenu::new(
                "子菜单",
                [MenuEntry::Action(MenuAction::new("inside", "内部"))],
            )),
        );
        let menu = harness.menu(
            entries,
            MenuOptions {
                max_height: px(200.0),
                ..options()
            },
        );
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.hover_row_of(&menu, "menu:panel", 2);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2, "子菜单打开");
        menu.inner
            .panel
            .list()
            .scroll_by(harness.app.app_mut(), px(600.0))
            .expect("滚动父菜单");
        harness.settle(&menu);
        assert!(
            !menu.is_open(harness.app.app()) && harness.stack().is_empty(),
            "父行滚出视口后锚点消失，菜单策略关闭整链"
        );
    }

    fn tall_submenu_entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("first", "第一")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "长",
                (0..8).map(|index| {
                    MenuEntry::Action(MenuAction::new(
                        format_smolstr!("item-{index}"),
                        format_smolstr!("条目 {index}"),
                    ))
                }),
            )),
        ]
    }

    fn open_tall_chain(harness: &mut Harness) -> (Menu, PopupSnapshotEntry, LogicalPoint) {
        let menu = harness.menu(tall_submenu_entries(), options());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2, "长子菜单打开");
        let root = harness.root_entry(&menu).id;
        let child = harness.submenu_entry(root);
        let panel = harness.bounds(menu.panel_node().expect("根面板"));
        let triangle = LogicalPoint::new(child.hit_rect.origin.x - 2.0, panel.max().y + 40.0);
        (menu, child, triangle)
    }

    #[test]
    fn a_press_inside_the_safe_triangle_keeps_the_chain_open() {
        let mut harness = Harness::new();
        let (menu, child, triangle) = open_tall_chain(&mut harness);
        let background_clicks = harness.background_clicks.get();
        let root_level = menu.inner.level.get().expect("根层级");
        assert!(
            menu.inner.sink.chain().safe_area_contains(triangle),
            "落点必须在安全三角/grace 内（否则该测试退化为普通外部按下）"
        );
        assert!(
            !menu
                .inner
                .sink
                .chain()
                .rect(root_level)
                .is_some_and(|rect| rect.contains(triangle))
        );
        harness.click(&menu, triangle);
        harness.settle(&menu);
        assert!(menu.is_open(harness.app.app()), "安全三角内按下不关链");
        assert_eq!(harness.stack().len(), 2, "根与子菜单都保留");
        assert_eq!(
            harness.submenu_entry(harness.root_entry(&menu).id).id,
            child.id,
            "子菜单仍是同一面板"
        );
        assert_eq!(
            harness.background_clicks.get() - background_clicks,
            0,
            "安全三角按下没有落到背景控件"
        );
    }

    #[test]
    fn a_press_outside_the_safe_triangle_closes_the_chain_and_replays() {
        let mut harness = Harness::new();
        let (menu, _child, _triangle) = open_tall_chain(&mut harness);
        assert!(
            !menu
                .inner
                .sink
                .chain()
                .safe_area_contains(LogicalPoint::new(px(640.0), px(520.0)))
        );
        let background_clicks = harness.background_clicks.get();
        harness.click(&menu, LogicalPoint::new(px(640.0), px(520.0)));
        harness.settle(&menu);
        assert!(!menu.is_open(harness.app.app()), "安全三角外按下关闭整链");
        assert!(harness.stack().is_empty());
        assert_eq!(
            harness.background_clicks.get() - background_clicks,
            1,
            "D14 默认 Replay：外部按下重放给页面"
        );
    }

    #[test]
    fn a_consuming_menu_does_not_replay_an_outside_press() {
        let mut harness = Harness::new();
        let menu = harness.menu(
            tall_submenu_entries(),
            MenuOptions {
                outside_press: OutsidePress::Consume,
                ..options()
            },
        );
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2);
        let background_clicks = harness.background_clicks.get();
        harness.click(&menu, LogicalPoint::new(px(640.0), px(520.0)));
        harness.settle(&menu);
        assert!(!menu.is_open(harness.app.app()));
        assert!(harness.stack().is_empty());
        assert_eq!(
            harness.background_clicks.get() - background_clicks,
            0,
            "显式 Consume 不重放外部按下"
        );
    }

    fn checked_state(menu: &Menu, id: &str) -> Option<bool> {
        menu.inner
            .entries
            .borrow()
            .iter()
            .find_map(|entry| match entry {
                MenuEntry::Action(action) if action.id == id => {
                    action.check.map(|check| check.checked)
                }
                _ => None,
            })
    }

    fn two_tall_submenus() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Action(MenuAction::new("first", "第一")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "长甲",
                (0..8).map(|index| {
                    MenuEntry::Action(MenuAction::new(
                        format_smolstr!("alpha-{index}"),
                        format_smolstr!("甲 {index}"),
                    ))
                }),
            )),
            MenuEntry::Submenu(MenuSubmenu::new(
                "长乙",
                (0..2).map(|index| {
                    MenuEntry::Action(MenuAction::new(
                        format_smolstr!("beta-{index}"),
                        format_smolstr!("乙 {index}"),
                    ))
                }),
            )),
        ]
    }

    #[test]
    fn a_cancelled_close_restores_the_chain_and_hover_cascade() {
        let mut harness = Harness::new();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()));
        harness.click(&menu, LogicalPoint::new(px(310.0), px(210.0)));
        assert!(menu.is_open(harness.app.app()), "opener Toggle 取消关闭");
        let level = menu.inner.level.get().expect("根层级");
        assert_eq!(
            menu.inner.sink.chain().state(level),
            Some(crate::kit::menu_chain::LevelState::Open),
            "取消关闭后链 root 层恢复 Open（不是 Closed）"
        );
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2, "重开后 hover 级联仍工作");
        assert_eq!(
            menu.inner.sink.chain().open_levels().len(),
            2,
            "重开后链层恢复且子层可开"
        );
    }

    #[test]
    fn a_failed_open_resets_the_chain_and_the_retry_keeps_submenus_working() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        let dead = harness.app.app_mut().create_window(WindowOptions {
            size: LogicalSize::new(px(800.0), px(600.0)),
            ..WindowOptions::default()
        });
        assert!(harness.app.app_mut().close_window(dead));
        let failed = menu.open(
            harness.app.app_mut(),
            dead,
            Anchor::WindowRect(LogicalRect::new(px(100.0), px(100.0), px(1.0), px(1.0))),
        );
        assert!(failed.is_err(), "已关闭窗口上 open 必须返回强类型错误");
        assert_eq!(menu.inner.level.get(), None, "失败后清空 MenuInner.level");
        assert!(
            menu.inner.sink.chain().open_levels().is_empty(),
            "失败后链已 reset"
        );

        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(harness.stack().len(), 2, "重试成功后键盘 Right 打开子菜单");
        assert_eq!(
            menu.inner.sink.chain().open_levels().len(),
            2,
            "重试后链层没有自引用槽"
        );
    }

    #[test]
    fn reentering_the_same_submenu_row_after_the_grace_close_reopens_it() {
        let mut harness = Harness::new();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        let root = harness.root_entry(&menu).id;
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2);
        let first = harness.submenu_entry(root).id;
        harness.hover_point(&menu, LogicalPoint::new(px(640.0), px(520.0)));
        harness.advance(&menu, Duration::from_millis(500));
        harness.settle(&menu);
        assert_eq!(harness.stack().len(), 1, "指针离开链后子菜单按 grace 关闭");
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(
            harness.stack().len(),
            2,
            "重入同一行必须重新启动 PendingOpen"
        );
        assert_ne!(
            harness.submenu_entry(root).id,
            first,
            "重开的是新的子菜单 popup"
        );
    }

    #[test]
    fn escape_cancels_a_pending_submenu_open() {
        let mut harness = Harness::new();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(150));
        assert!(
            menu.inner.pending_open.get().is_some(),
            "PendingOpen 已 armed"
        );
        harness.key(&menu, KeyCode::Escape);
        assert!(
            menu.inner.pending_open.get().is_none(),
            "Esc 进入 Closing 后立即取消 PendingOpen"
        );
        for _ in 0..12 {
            harness.advance(&menu, Duration::from_millis(20));
            assert!(
                harness.stack().len() <= 1,
                "关闭动画期间子菜单不得闪现（stack={}）",
                harness.stack().len()
            );
        }
        harness.settle(&menu);
        assert!(harness.stack().is_empty(), "根随后关闭完成");
    }

    #[test]
    fn the_safe_triangle_expires_and_the_sibling_replaces_it() {
        let mut harness = Harness::new();
        let menu = harness.menu(two_tall_submenus(), options());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        let first = harness.submenu_entry(harness.root_entry(&menu).id).id;
        let root_level = menu.inner.level.get().expect("根层级");
        let child_level = menu.inner.submenus.borrow()[0].level;
        let child_rect = menu
            .inner
            .sink
            .chain()
            .rect(child_level)
            .expect("子菜单矩形");
        let row2 = harness.screen_bounds(harness.row(2));
        let triangle = LogicalPoint::new(child_rect.origin.x - 2.0, row2.origin.y + 2.0);
        harness.hover_point(&menu, triangle);
        assert!(
            menu.inner.sink.chain().grace(child_level).is_some(),
            "指针移出触发行后 grace 三角形建立"
        );
        harness.advance(&menu, Duration::from_millis(350));
        assert!(
            menu.inner.sink.chain().grace(child_level).is_none(),
            "300ms 后 safe triangle 过期"
        );
        harness.hover_row_of(&menu, "menu:panel", 2);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2, "兄弟菜单打开");
        let replaced = harness.submenu_entry(harness.root_entry(&menu).id).id;
        assert_ne!(replaced, first, "过期的三角不再阻止兄弟替换");
        let new_level = menu.inner.submenus.borrow()[0].level;
        assert_ne!(new_level, child_level, "替换行分配新的层级槽");
        assert_eq!(
            menu.inner.sink.chain().child(root_level),
            Some(new_level),
            "链重新挂到新子层"
        );
        assert_eq!(
            menu.inner.sink.chain().active(root_level),
            Some(2),
            "父层 active 跟随替换行"
        );
    }

    #[test]
    fn disabled_submenu_rows_never_open() {
        let mut harness = Harness::new();
        let entries = vec![
            MenuEntry::Action(MenuAction::new("plain", "普通")),
            MenuEntry::Submenu(
                MenuSubmenu::new(
                    "禁用更多",
                    [MenuEntry::Action(MenuAction::new("inner", "内部"))],
                )
                .disabled(true),
            ),
        ];
        let menu = harness.menu(entries, options());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(300));
        assert_eq!(harness.stack().len(), 1, "禁用子菜单行 hover 不打开");
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(harness.stack().len(), 1, "禁用子菜单行 Right 不打开");
    }

    #[test]
    fn shift_tab_closes_the_chain_and_focuses_the_previous_node() {
        let mut harness = Harness::new();
        let before = focusable_node(
            &mut harness,
            "tab-before",
            LogicalRect::new(px(600.0), px(40.0), px(80.0), px(32.0)),
        );
        let opener = focusable_node(
            &mut harness,
            "tab-opener",
            LogicalRect::new(px(600.0), px(90.0), px(80.0), px(32.0)),
        );
        let menu = harness.menu(entries(), options());
        let _ = menu.open(
            harness.app.app_mut(),
            harness.window,
            Anchor::Node { node: opener },
        );
        harness.settle(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key_with(&menu, KeyCode::Tab, Modifiers::SHIFT);
        harness.settle(&menu);
        assert!(!menu.is_open(harness.app.app()), "Shift+Tab 关闭整链");
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(before),
            "焦点移到 opener 之前的节点"
        );
        assert_ne!(harness.app.app().focused(harness.window), Some(opener));
    }

    #[test]
    fn radio_groups_are_bounded_by_separators() {
        let mut harness = Harness::new();
        let entries = vec![
            MenuEntry::Action(MenuAction::new("r1", "一").radio(true)),
            MenuEntry::Action(MenuAction::new("r2", "二").radio(false)),
            MenuEntry::Separator,
            MenuEntry::Action(MenuAction::new("r3", "三").radio(false)),
            MenuEntry::Action(MenuAction::new("r4", "四").radio(true)),
        ];
        let menu = harness.menu(entries, options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::ArrowDown);
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
        harness.key(&menu, KeyCode::Space);
        assert_eq!(
            checked_state(&menu, "r2"),
            Some(true),
            "空格勾选第二组第一项"
        );
        assert_eq!(checked_state(&menu, "r1"), Some(false));
        assert_eq!(
            checked_state(&menu, "r4"),
            Some(true),
            "分隔线之后是另一组，r4 保持勾选"
        );
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::Space);
        assert_eq!(checked_state(&menu, "r3"), Some(true), "勾选第二组的 r3");
        assert_eq!(checked_state(&menu, "r4"), Some(false), "同组内 r4 被取消");
        assert_eq!(checked_state(&menu, "r2"), Some(true), "另一组保持勾选");
        assert!(menu.is_open(harness.app.app()), "radio 空格不关菜单");
    }

    #[test]
    fn the_first_keyboard_event_moves_focus_into_the_hover_opened_child() {
        let mut harness = Harness::new();
        let entries = vec![
            MenuEntry::Action(MenuAction::new("plain", "普通")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [
                    MenuEntry::Action(MenuAction::new("inner", "内部")),
                    MenuEntry::Action(MenuAction::new("second", "第二")),
                ],
            )),
        ];
        let menu = harness.menu(entries, options());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        let level2 = menu.inner.submenus.borrow()[0].menu.clone();
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:panel:surface"),
            "hover 打开后焦点仍在根"
        );
        assert_eq!(level2.highlighted(harness.app.app()), Some(0));
        harness.key(&menu, KeyCode::ArrowDown);
        assert_eq!(
            harness.focused_debug_id().as_deref(),
            Some("menu:sub:1:panel:surface"),
            "首个键盘事件把焦点移入 hover 打开的子层"
        );
        assert_eq!(
            level2.highlighted(harness.app.app()),
            Some(1),
            "移动落在此前 hover 的子层，而不是根"
        );
        assert_eq!(menu.highlighted(harness.app.app()), Some(1));
    }

    #[test]
    fn rebinding_the_trigger_replaces_the_previous_action() {
        let mut harness = Harness::new();
        let trigger = focusable_node(
            &mut harness,
            "rebind-trigger",
            LogicalRect::new(px(200.0), px(40.0), px(80.0), px(32.0)),
        );
        let menu = harness.menu(entries(), options());
        menu.bind_trigger(harness.app.app_mut(), harness.window, trigger)
            .expect("首次绑定");
        menu.bind_trigger(harness.app.app_mut(), harness.window, trigger)
            .expect("重复绑定");
        let _ = harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(trigger));
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::F10, Modifiers::ALT));
        let _ = harness
            .app
            .inject(InputEvent::key_up(KeyCode::F10, Modifiers::ALT));
        harness.settle(&menu);
        assert!(
            menu.is_open(harness.app.app()),
            "重复绑定后 Alt+F10 仍只开一次"
        );
        assert_eq!(harness.stack().len(), 1);
    }
    #[test]
    fn a_submenu_cannot_replace_the_shared_activation_sink() {
        let mut harness = Harness::new();
        let root_calls: Rc<RefCell<Vec<SmolStr>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = root_calls.clone();
        let menu = harness
            .menu(three_level_entries(), options())
            .on_activate(move |_, activation| sink.borrow_mut().push(activation.id));
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        let level2 = menu.inner.submenus.borrow()[0].menu.clone();
        let ignored: Rc<RefCell<Vec<SmolStr>>> = Rc::new(RefCell::new(Vec::new()));
        let ignored_sink = ignored.clone();
        let level2 = level2.on_activate(move |_, activation| {
            ignored_sink.borrow_mut().push(activation.id);
        });
        harness.click(&menu, harness.center_of("menu:sub:1:panel:list:row:0"));
        assert_eq!(
            root_calls.borrow().as_slice(),
            &[SmolStr::new_static("inner")],
            "子层 on_activate 不覆盖共享 sink，根的 callback 仍生效"
        );
        assert!(ignored.borrow().is_empty(), "子层 callback 不被调用");
        assert!(!level2.is_open(harness.app.app()), "菜单链已关闭");
    }

    #[test]
    fn a_checked_entry_reports_its_check_state() {
        let mut harness = Harness::new();
        let reported: Rc<RefCell<Vec<(SmolStr, Option<MenuCheckState>)>>> =
            Rc::new(RefCell::new(Vec::new()));
        let sink = reported.clone();
        let menu = harness
            .menu(
                vec![MenuEntry::Action(
                    MenuAction::new("toggle", "开关").checkbox(false),
                )],
                options(),
            )
            .on_activate(move |_, activation| {
                sink.borrow_mut().push((activation.id, activation.check));
            });
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::Space);
        let reported = reported.borrow();
        assert_eq!(reported.len(), 1, "空格切换一次只上报一次");
        assert_eq!(reported[0].0, SmolStr::new_static("toggle"));
        assert_eq!(
            reported[0].1,
            Some(MenuCheckState {
                kind: MenuCheck::Checkbox,
                checked: true,
            }),
            "激活上报 new check 状态"
        );
    }
    #[test]
    fn escaping_a_hover_opened_submenu_then_reentering_the_row_reopens_it() {
        let mut harness = Harness::new();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        let root = harness.root_entry(&menu).id;
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        assert_eq!(harness.stack().len(), 2);
        harness.key(&menu, KeyCode::Escape);
        harness.hover_point(&menu, LogicalPoint::new(px(700.0), px(520.0)));
        harness.settle(&menu);
        assert_eq!(harness.stack().len(), 1, "Esc 只关子菜单");
        assert!(menu.is_open(harness.app.app()), "根保留");
        assert!(harness.submenu_entry_opt(root).is_none());
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(300));
        assert_eq!(
            harness.stack().len(),
            2,
            "移出再回同一 submenu 行必须重新打开（M2-02R）"
        );
    }

    #[test]
    fn a_programmatic_reopen_after_two_escapes_reopens_the_submenu_row() {
        let mut harness = Harness::new();
        let menu = harness.menu(submenu_entries(), options());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        harness.key(&menu, KeyCode::Escape);
        harness.settle(&menu);
        assert!(!menu.is_open(harness.app.app()), "两次 Esc 关闭整链");
        assert!(harness.stack().is_empty());
        harness.open(&menu);
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(300));
        assert_eq!(
            harness.stack().len(),
            2,
            "整链关闭+程序化重开后同一行仍可 hover 打开（M2-02R）"
        );
    }

    #[test]
    fn submenus_opened_during_the_root_opening_animation_survive() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        let anchor = Anchor::WindowRect(LogicalRect::from_origin_size(
            LogicalPoint::new(px(360.0), px(140.0)),
            LogicalSize::new(px(1.0), px(1.0)),
        ));
        assert!(
            menu.open(harness.app.app_mut(), harness.window, anchor)
                .expect("打开根菜单")
        );
        let level2 = menu
            .open_submenu(harness.app.app_mut(), 1)
            .expect("二级")
            .expect("二级句柄");
        let _level3 = level2
            .open_submenu(harness.app.app_mut(), 1)
            .expect("三级")
            .expect("三级句柄");
        assert_eq!(menu.inner.sink.chain().open_levels().len(), 3);
        harness.settle(&menu);
        assert_eq!(
            harness.stack().len(),
            3,
            "root Opening 期间打开的子菜单不得因 anchor Moved 关闭整链"
        );
        assert!(menu.is_open(harness.app.app()));
        assert_eq!(menu.inner.sink.chain().open_levels().len(), 3);
    }

    #[test]
    fn a_flipped_submenu_keeps_the_chain_through_the_mirrored_safe_triangle() {
        let mut harness = Harness::new();
        let menu = harness.menu(sibling_entries(), options());
        let anchor = Anchor::WindowRect(LogicalRect::from_origin_size(
            LogicalPoint::new(px(650.0), px(120.0)),
            LogicalSize::new(px(1.0), px(1.0)),
        ));
        assert!(
            menu.open(harness.app.app_mut(), harness.window, anchor)
                .expect("打开")
        );
        harness.settle(&menu);
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.max().x.value() <= 800.5,
            "根面板必须在视口内：{panel:?}"
        );
        harness.hover_row_of(&menu, "menu:panel", 1);
        harness.advance(&menu, Duration::from_millis(220));
        let root = harness.root_entry(&menu).id;
        let child = harness.submenu_entry(root);
        assert!(
            child.hit_rect.max().x <= panel.origin.x + 1.0,
            "空间不足时子菜单翻到左侧：child={:?} panel={:?}",
            child.hit_rect,
            panel
        );
        let child_level = menu.inner.submenus.borrow()[0].level;
        let at = LogicalPoint::new(panel.origin.x + 2.0, panel.max().y - 3.0);
        harness.hover_point(&menu, at);
        let grace = menu
            .inner
            .sink
            .chain()
            .grace(child_level)
            .expect("翻面后 grace 三角存在");
        let apex = grace.points()[0];
        assert!(
            apex.x > child.hit_rect.max().x,
            "左翻时三角顶点在子菜单右缘右侧（镜像 bleed）：apex={apex:?} child={:?}",
            child.hit_rect
        );
        harness.advance(&menu, Duration::from_millis(400));
        assert_eq!(harness.stack().len(), 2, "镜像三角/grace 内 400ms 保持链");
        assert_eq!(
            harness.submenu_entry(harness.root_entry(&menu).id).id,
            child.id
        );
    }

    #[test]
    fn a_stale_typeahead_timer_does_not_clear_a_newer_layers_buffer() {
        let mut harness = Harness::new();
        let menu = harness.menu(typeahead_entries(), options());
        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::KeyD);
        assert_eq!(
            menu.inner.sink.chain().typeahead_buffer(),
            SmolStr::new_static("d"),
            "根层输入 d 匹配 Delta 子菜单"
        );
        harness.advance(&menu, Duration::from_millis(500));
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(harness.stack().len(), 2, "右侧打开 Delta 子菜单");
        harness.key(&menu, KeyCode::KeyD);
        assert_eq!(
            menu.inner.sink.chain().typeahead_buffer(),
            SmolStr::new_static("d"),
            "子层输入 d"
        );
        harness.advance(&menu, Duration::from_millis(1600));
        assert_eq!(
            menu.inner.sink.chain().typeahead_buffer(),
            SmolStr::new_static("d"),
            "根层旧 timer 先到期时不得清空子层 buffer（R2-a）"
        );
        harness.advance(&menu, Duration::from_millis(500));
        assert!(
            menu.inner.sink.chain().typeahead_buffer().is_empty(),
            "子层自己的 timer 到期后清空"
        );
    }
    #[test]
    fn a_failed_reopen_with_an_existing_level_resets_the_chain() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        let level = menu
            .inner
            .sink
            .chain()
            .open_root(None, crate::kit::menu_chain::OpenSource::Click);
        menu.inner.level.set(Some(level));
        assert!(menu.inner.level.get().is_some(), "构造 level 已存在");
        let root = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .root();
        let gone = NodeBuilder::boxed()
            .id("gone-anchor")
            .layout_override(LogicalRect::new(px(100.0), px(100.0), px(1.0), px(1.0)))
            .build(
                harness
                    .app
                    .app_mut()
                    .window_mut(harness.window)
                    .expect("窗口")
                    .tree_mut(),
                root,
            )
            .expect("临时节点");
        harness
            .app
            .app_mut()
            .window_mut(harness.window)
            .expect("窗口")
            .tree_mut()
            .remove(gone)
            .expect("移除临时节点");
        let failed = menu.open(
            harness.app.app_mut(),
            harness.window,
            Anchor::Node { node: gone },
        );
        assert!(
            failed.is_err(),
            "level 已存在时 panel.open 失败必须返回 Err：{failed:?}"
        );
        assert_eq!(
            menu.inner.level.get(),
            None,
            "失败分支清空已存在的 MenuInner.level"
        );
        assert!(
            menu.inner.sink.chain().open_levels().is_empty(),
            "失败分支 reset 链"
        );

        harness.open(&menu);
        harness.focus(&menu, menu.inner.panel.surface_node().expect("surface"));
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(
            harness.stack().len(),
            2,
            "失败重试后键盘 Right 正常打开子菜单"
        );
        assert_eq!(menu.inner.sink.chain().open_levels().len(), 2);
    }

    #[test]
    fn the_derived_safe_triangle_covers_the_narrow_gap_without_stored_grace() {
        let mut harness = Harness::new();
        let (menu, _child, triangle) = open_tall_chain(&mut harness);
        let child_level = menu.inner.submenus.borrow()[0].level;
        let chain = menu.inner.sink.chain();
        assert!(
            chain.grace(child_level).is_none(),
            "指针停在触发行上时无存储 grace"
        );
        assert!(
            chain.grace_contains(triangle).is_none(),
            "该点不是 grace 覆盖的（隔离 grace 路径）"
        );
        assert!(
            chain.safe_area_contains(triangle),
            "derived safe triangle 覆盖 gap=0 的窄带落点"
        );
        assert!(
            chain.contains(triangle),
            "derived triangle 同时计入链 containment，避免按下后误 pointer-leave"
        );
        harness.click(&menu, triangle);
        harness.settle(&menu);
        assert_eq!(
            harness.stack().len(),
            2,
            "非 grace 的 derived 三角也 Ignore 外部按下"
        );
        assert!(menu.is_open(harness.app.app()));
    }

    #[test]
    fn a_pointer_close_pending_level_is_not_revived_by_reconcile() {
        let mut harness = Harness::new();
        let menu = harness.menu(three_level_entries(), options());
        open_three_levels(&mut harness, &menu);
        harness.hover_point(&menu, LogicalPoint::new(px(700.0), px(520.0)));
        harness.advance(&menu, Duration::from_millis(100));
        let chain = menu.inner.sink.chain();
        let deepest = *chain.open_levels().last().expect("最深链层");
        assert_eq!(
            chain.state(deepest),
            Some(crate::kit::menu_chain::LevelState::Closing),
            "指针离开后 hover 层进入 Closing"
        );
        harness.advance(&menu, Duration::from_millis(16));
        assert_eq!(
            chain.state(deepest),
            Some(crate::kit::menu_chain::LevelState::Closing),
            "reconcile_chain 不得把 pointer-close 的 Closing 层复活"
        );
        harness.advance(&menu, Duration::from_millis(300));
        assert_eq!(harness.stack().len(), 1, "grace 到期后只关 hover 层");
        assert!(menu.is_open(harness.app.app()), "click-opened 根保留");
    }
}
