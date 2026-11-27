use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
    time::Duration,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yPatch, A11yRole},
    app::{App, TimerId},
    error::EngineError,
    event::{Action, ActionSource, KeyBinding, KeyCode, Modifiers},
    interaction::ItemAlign,
    math::{LogicalPx, px},
    node::{
        builder::NodeBuilder, dependency::ReadCtx, id::NodeId, image::ImageFit, signal::Signal,
        tree::NodeTree,
    },
    paint::{style::StylePatch, style_builder::StyleBuilder},
    popup::{
        Alignment, Anchor, DismissPolicy, DismissReason, EscapePolicy, FocusPolicy, HasPopup,
        OutsidePress, PlacementSpec, PopupA11y, PopupAnimation, PopupError, PopupId, PopupSpec,
        PopupState, PopupTier, RestoreFocus, Side,
    },
    resources::ResourceId,
    text::{TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    kit::{
        icon::{ICON_INTRINSIC_SIZE, IconKind, svg},
        list::{List, ListOptions, RowHeight, Selection, SelectionMode},
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
const SUBMENU_DELAY: Duration = Duration::from_millis(200);
const MENU_ROLES: PanelRoles = PanelRoles {
    panel: Some(A11yRole::Menu),
    row: A11yRole::MenuItem,
};
const MENU_DISMISS: DismissPolicy = DismissPolicy::Chain {
    outside: OutsidePress::Replay,
    escape: EscapePolicy::CloseTopmost,
    anchor_lost: true,
    focus_lost: true,
};

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
                let _ = panel.activate(app, index);
            }
        });
        panel.register_actions(app);
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
            self.select_first(app);
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
        let activate = self.inner.actions.activate.clone();
        let surface_slot: Rc<Cell<Option<NodeId>>> = Rc::new(Cell::new(None));
        let slot = surface_slot.clone();
        let mut spec = PopupSpec::new(request.anchor, self.inner.options.placement.clone());
        spec.parent = request.parent;
        spec.tier = self.inner.options.tier;
        spec.dismiss = self.inner.options.dismiss;
        spec.focus = request.focus;
        spec.opener = request.opener;
        spec.animation = PopupAnimation::Fade;
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
                    NodeBuilder::boxed()
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
                        .on_click(activate)
                        .a11y(A11yOverride {
                            role: Some(A11yRole::GenericContainer),
                            ..A11yOverride::default()
                        })
                        .build(target.tree_mut(), body)?
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
            })
            .and_then(|()| {
                self.bind_keys(app, window)
                    .map_err(|source| PopupError::ContentBuild {
                        node: surface,
                        source,
                    })
            });
        if let Err(error) = wired {
            let _ = self.inner.panel.close_now(app);
            return Err(error);
        }
        self.select_first(app);
        Ok(true)
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

    pub(crate) fn close_now(&self, app: &mut App) -> bool {
        self.inner.panel.close_now(app)
    }

    pub(crate) fn on_activate(&self, callback: impl Fn(&mut App, usize) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Rc::new(callback));
    }

    pub(crate) fn on_close(&self, callback: impl Fn(&mut App, DismissReason) + 'static) {
        *self.inner.on_close.borrow_mut() = Some(Rc::new(callback));
    }

    pub(crate) fn set_highlighted(&self, app: &mut App, index: Option<usize>) {
        let index = index.filter(|index| !self.row_disabled(*index));
        let mut selection = Selection::new();
        if let Some(index) = index {
            selection.select(index);
        }
        if app.write(&self.inner.list.selection_signal(), selection)
            && let Some(index) = index
        {
            let _ = self
                .inner
                .list
                .scroll_to_item(app, index, ItemAlign::Nearest);
        }
    }

    pub(crate) fn select_first(&self, app: &mut App) {
        let next = (0..self.row_count()).find(|index| !self.row_disabled(*index));
        self.set_highlighted(app, next);
    }

    pub(crate) fn activate(&self, app: &mut App, index: usize) -> bool {
        if self.row_disabled(index) || index >= self.row_count() {
            return false;
        }
        self.set_highlighted(app, Some(index));
        let callback = self.inner.on_activate.borrow().clone();
        match callback {
            Some(callback) => {
                callback(app, index);
                true
            }
            None => false,
        }
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

    fn register_actions(&self, app: &mut App) {
        let this = self.clone();
        app.on_action(self.inner.actions.up.clone(), move |ctx| {
            this.move_highlight(ctx.app_mut(), -1);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.down.clone(), move |ctx| {
            this.move_highlight(ctx.app_mut(), 1);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.home.clone(), move |ctx| {
            this.jump_highlight(ctx.app_mut(), false);
        });
        let this = self.clone();
        app.on_action(self.inner.actions.end.clone(), move |ctx| {
            this.jump_highlight(ctx.app_mut(), true);
        });
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

    fn bind_keys(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let Some(surface) = self.inner.state.borrow().surface else {
            return Ok(());
        };
        let bindings = [
            (KeyCode::ArrowUp, self.inner.actions.up.clone()),
            (KeyCode::ArrowDown, self.inner.actions.down.clone()),
            (KeyCode::Home, self.inner.actions.home.clone()),
            (KeyCode::End, self.inner.actions.end.clone()),
        ];
        for (key, action) in bindings {
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

    fn move_highlight(&self, app: &mut App, step: isize) {
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

    fn jump_highlight(&self, app: &mut App, last: bool) {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuAction {
    pub id: SmolStr,
    pub label: SmolStr,
    pub icon: Option<IconKind>,
    pub shortcut: Option<SmolStr>,
    pub disabled: bool,
    pub danger: bool,
    pub checked: Option<bool>,
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
            checked: None,
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
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
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
    pub submenu_open_delay: Duration,
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
            submenu_open_delay: SUBMENU_DELAY,
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
    pub fn submenu_open_delay(mut self, delay: Duration) -> Self {
        self.submenu_open_delay = delay;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuActivation {
    pub id: SmolStr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowKind {
    Action(usize),
    Submenu(usize),
    Group(usize),
    Separator,
}

#[derive(Default)]
struct MenuSink {
    on_activate: RefCell<Option<Rc<dyn Fn(&mut App, MenuActivation)>>>,
    on_close: RefCell<Option<Rc<dyn Fn(&mut App, DismissReason)>>>,
}

struct OpenSubmenu {
    row: usize,
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
    submenu_timer: Cell<Option<TimerId>>,
    last_hovered: Cell<Option<usize>>,
    window: Cell<Option<WindowId>>,
    watch: RefCell<AnchorWatch>,
    scroll_closes: Cell<bool>,
    internal_scroll: Cell<Option<LogicalPx>>,
    right_action: Action,
    left_action: Action,
}

impl Menu {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        entries: impl IntoIterator<Item = MenuEntry>,
        options: MenuOptions,
    ) -> Self {
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
                sink: Rc::new(MenuSink::default()),
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
                dismiss: MENU_DISMISS,
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
            submenu_timer: Cell::new(None),
            last_hovered: Cell::new(None),
            window: Cell::new(None),
            watch: RefCell::new(AnchorWatch::default()),
            scroll_closes: Cell::new(false),
            internal_scroll: Cell::new(None),
            right_action: Action::Custom(format_smolstr!("{key}.right")),
            left_action: Action::Custom(format_smolstr!("{key}.left")),
        });
        let menu = Self { inner };
        menu.install_content(app);
        menu.install_handlers();
        menu
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
        self.open_focused(app, window, anchor, true)
    }

    fn open_focused(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        take_focus: bool,
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
            Anchor::WindowRect(_) | Anchor::NodeRect { .. } => None,
        };
        self.inner.watch.borrow_mut().reset();
        self.inner.internal_scroll.set(None);
        let opened = self.inner.panel.open(
            app,
            window,
            PanelOpen {
                anchor,
                opener,
                focus,
                parent,
                width: self.inner.options.width,
                a11y_parent: self.inner.a11y_parent,
            },
        )?;
        if opened && let Err(source) = self.bind_extra_keys(app, window) {
            let error = popup_error(app, window, source);
            self.close_now(app);
            return Err(error);
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
        }
        Ok(opened)
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
        *self.inner.sink.on_activate.borrow_mut() = Some(Rc::new(callback));
        self
    }

    pub fn on_close(self, callback: impl Fn(&mut App, DismissReason) + 'static) -> Self {
        *self.inner.sink.on_close.borrow_mut() = Some(Rc::new(callback));
        self
    }

    pub(crate) fn closing_on_scroll(self) -> Self {
        self.inner.scroll_closes.set(true);
        self
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let mut changed = self.inner.panel.sync(app)?;
        if !self.inner.panel.is_open(app) {
            self.close_submenus(app);
            return Ok(changed);
        }
        if self.sync_anchor(app) {
            self.close_submenus(app);
            return Ok(true);
        }
        changed |= self.sync_hover(app)?;
        let submenus: Vec<Menu> = self
            .inner
            .submenus
            .borrow()
            .iter()
            .map(|open| open.menu.clone())
            .collect();
        for submenu in &submenus {
            changed |= submenu.sync(app)?;
        }
        self.retain_open_submenus(app);
        Ok(changed)
    }

    pub fn release(&self, app: &mut App) -> usize {
        self.close_submenus(app);
        if let Some(timer) = self.inner.submenu_timer.take() {
            app.cancel_timer(&timer);
        }
        app.off_action(&self.inner.right_action);
        app.off_action(&self.inner.left_action);
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

    fn parent_popup_id(&self) -> Option<PopupId> {
        let (parent, _) = self.inner.parent.as_ref()?;
        let parent = Menu {
            inner: parent.upgrade()?,
        };
        parent.inner.panel.id()
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
                        if let Some(checked) = action.checked {
                            build_check(tree, parent, &theme, checked, size)?;
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
                    role: Some(MENU_ROLES.row),
                    label: Some(action.label.clone()),
                    disabled: Some(action.disabled),
                    checked: action.checked,
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
                    role: Some(A11yRole::Label),
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

    fn install_handlers(&self) {
        let this = self.clone();
        self.inner.panel.on_activate(move |app, row| {
            this.handle_row(app, row);
        });
        let this = self.clone();
        self.inner.panel.on_close(move |app, reason| {
            this.close_submenus(app);
            this.fire_close(app, reason);
        });
    }

    fn handle_row(&self, app: &mut App, row: usize) {
        let kind = self.inner.rows.borrow().get(row).copied();
        match kind {
            Some(RowKind::Submenu(index)) => {
                self.open_submenu(app, row, index, true);
            }
            Some(RowKind::Action(index)) => {
                let entries = self.inner.entries.borrow().clone();
                let Some(MenuEntry::Action(action)) = entries.get(index) else {
                    return;
                };
                if action.disabled {
                    return;
                }
                let activation = MenuActivation {
                    id: action.id.clone(),
                };
                if let Some(callback) = self.inner.sink.on_activate.borrow().clone() {
                    callback(app, activation);
                }
                self.close_chain(app);
            }
            Some(RowKind::Group(_) | RowKind::Separator) | None => {}
        }
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

    fn close_chain(&self, app: &mut App) {
        let root = self.root();
        let _ = root.inner.panel.close(app);
    }

    fn open_submenu(&self, app: &mut App, row: usize, index: usize, focus: bool) {
        if self
            .inner
            .submenus
            .borrow()
            .iter()
            .any(|open| open.row == row && open.menu.is_open(app))
        {
            return;
        }
        let entries = self.inner.entries.borrow().clone();
        let Some(MenuEntry::Submenu(submenu)) = entries.get(index) else {
            return;
        };
        if submenu.disabled {
            return;
        }
        let (Some(window), Some(row_node)) = (
            self.inner.window.get(),
            self.inner.panel.list().row_node(app, row),
        ) else {
            return;
        };
        self.close_submenus(app);
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
            },
        );
        let anchor = Anchor::Node { node: row_node };
        match child.open_focused(app, window, anchor, focus) {
            Ok(true) => {
                self.inner
                    .submenus
                    .borrow_mut()
                    .push(OpenSubmenu { row, menu: child });
            }
            Ok(false) => {
                let _ = child.release(app);
            }
            Err(_) => {
                let _ = child.release(app);
            }
        }
    }

    fn close_submenus(&self, app: &mut App) {
        let submenus: Vec<OpenSubmenu> = self.inner.submenus.borrow_mut().drain(..).collect();
        for open in submenus.into_iter().rev() {
            open.menu.close_now(app);
            let _ = open.menu.release(app);
        }
        if let Some(timer) = self.inner.submenu_timer.take() {
            app.cancel_timer(&timer);
        }
        self.inner.last_hovered.set(None);
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
            let _ = open.menu.release(app);
        }
    }

    fn sync_anchor(&self, app: &mut App) -> bool {
        let (Some(id), Some(window)) = (self.inner.panel.id(), self.inner.window.get()) else {
            return false;
        };
        let change = self.inner.watch.borrow_mut().observe(app, id, window);
        let internal_scroll = self.sync_internal_scroll(app);
        match change {
            AnchorChange::Lost | AnchorChange::Resized | AnchorChange::Moved => {
                self.close_chain(app);
                true
            }
            AnchorChange::Scrolled if self.inner.scroll_closes.get() && !internal_scroll => {
                self.close_chain(app);
                true
            }
            AnchorChange::None | AnchorChange::Scrolled => false,
        }
    }

    fn sync_internal_scroll(&self, app: &App) -> bool {
        let offset = self.inner.panel.list().offset(app);
        let changed = offset.is_some() && offset != self.inner.internal_scroll.get();
        self.inner.internal_scroll.set(offset);
        changed
    }

    fn sync_hover(&self, app: &mut App) -> Result<bool, EngineError> {
        let Some(window) = self.inner.window.get() else {
            return Ok(false);
        };
        let hovered = self.inner.panel.hovered(app);
        if hovered == self.inner.last_hovered.get() {
            return Ok(false);
        }
        self.inner.last_hovered.set(hovered);
        if let Some(timer) = self.inner.submenu_timer.take() {
            app.cancel_timer(&timer);
        }
        let kind = hovered.and_then(|row| {
            self.inner
                .rows
                .borrow()
                .get(row)
                .copied()
                .map(|kind| (row, kind))
        });
        match kind {
            Some((row, RowKind::Submenu(index))) => {
                let delay = self.inner.options.submenu_open_delay;
                if delay.is_zero() {
                    self.open_submenu(app, row, index, false);
                } else {
                    let this = self.clone();
                    let timer = app.after(window, delay, move |app| {
                        if this.inner.last_hovered.get() == Some(row) {
                            this.open_submenu(app, row, index, false);
                        }
                    })?;
                    self.inner.submenu_timer.set(Some(timer));
                }
            }
            Some((_, RowKind::Action(_) | RowKind::Group(_) | RowKind::Separator)) => {
                self.close_submenus(app);
            }
            None => {}
        }
        Ok(true)
    }

    fn bind_extra_keys(&self, app: &mut App, window: WindowId) -> Result<(), EngineError> {
        let Some(surface) = self.inner.panel.surface_node() else {
            return Ok(());
        };
        for (key, action) in [
            (KeyCode::ArrowRight, self.inner.right_action.clone()),
            (KeyCode::ArrowLeft, self.inner.left_action.clone()),
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
        let this = self.clone();
        app.on_action(self.inner.right_action.clone(), move |ctx| {
            if let Some(row) = this.inner.panel.highlighted(ctx.app_mut())
                && let Some(RowKind::Submenu(index)) = this.inner.rows.borrow().get(row).copied()
            {
                let app = ctx.app_mut();
                this.open_submenu(app, row, index, true);
            }
        });
        let this = self.clone();
        app.on_action(self.inner.left_action.clone(), move |ctx| {
            if this.inner.kind != MenuKind::Submenu {
                return;
            }
            let app = ctx.app_mut();
            this.close_now(app);
            let parent = this
                .inner
                .parent
                .as_ref()
                .and_then(|(parent, row)| parent.upgrade().map(|parent| (parent, *row)));
            if let Some((parent, row)) = parent {
                let parent = Menu { inner: parent };
                parent.inner.panel.set_highlighted(app, Some(row));
            }
        });
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
    checked: bool,
    size: UnifiedSize,
) -> Result<(), EngineError> {
    let theme = theme.clone();
    NodeBuilder::text(
        if checked { "✓" } else { " " },
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

    use smol_str::SmolStr;
    use ui_core::{
        a11y::{A11yAction, A11yRole},
        event::{Action, ActionSource, InputEvent, KeyCode, Modifiers, PointerButton},
        interaction::ScrollConfig,
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::style_builder::StyleBuilder,
        popup::{Anchor, HasPopup, PopupId, PopupSnapshotEntry, PopupState},
        test_support::{app::TestApp, record::FrameRecord},
        window::{WindowId, WindowOptions},
    };

    use super::{Menu, MenuAction, MenuEntry, MenuOptions, MenuSubmenu};
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
}
