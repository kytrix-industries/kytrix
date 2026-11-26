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
    resources::ResourceId,
    text::{TextStyle, TextWrap},
    window::WindowId,
};

use crate::{
    kit::{
        icon::{ICON_INTRINSIC_SIZE, IconKind, svg},
        layer::{Band, Layer, LayerSlot},
        list::{List, ListOptions, RowHeight, Selection, SelectionMode},
        overlay::{
            Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayA11y, OverlayError,
            OverlayOptions, OverlaySurface, Placement,
        },
        size::UnifiedSize,
    },
    theme::{handle::ThemeHandle, role::Role},
};

const MENU_PADDING: LogicalPx = LogicalPx::new(4.0);
const GROUP_HEIGHT_RATIO: f32 = 0.75;
const SEPARATOR_HEIGHT: LogicalPx = LogicalPx::new(9.0);
const SUBMENU_GAP: LogicalPx = LogicalPx::ZERO;
const SUBMENU_DELAY: Duration = Duration::from_millis(150);

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
    pub band: Band,
    pub modal: bool,
    pub placement: Placement,
    pub gap: LogicalPx,
    pub roles: PanelRoles,
    pub label: Option<SmolStr>,
    pub width: PanelWidth,
    pub slot: LayerSlot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelWidth {
    Exact,
    AtLeast,
}

type PanelResult<T> = Result<T, OverlayError>;

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
    width: LogicalPx,
    min_width: LogicalPx,
    content_width: LogicalPx,
    measured: bool,
    measure: Option<NodeId>,
    height: LogicalPx,
    opened: bool,
}

struct PanelInner {
    key: SmolStr,
    options: ItemPanelOptions,
    overlay: Overlay,
    list: List,
    content: Rc<RefCell<Option<PanelContent>>>,
    actions: PanelActions,
    on_activate: RefCell<Option<Rc<dyn Fn(&mut App, usize)>>>,
    on_close: RefCell<Option<Rc<dyn Fn(&mut App)>>>,
    state: RefCell<PanelState>,
    open: Signal<bool>,
}

#[derive(Clone)]
pub(crate) struct ItemPanel {
    inner: Rc<PanelInner>,
}

impl ItemPanel {
    pub(crate) fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
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
            content_width: None,
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
        let overlay = Overlay::new(
            app,
            theme,
            layer,
            router,
            key.clone(),
            OverlayOptions {
                placement: options.placement,
                gap: options.gap,
                margin: px(4.0),
                flip: true,
                dismiss: DismissPolicy::OutsideClick,
                modal: options.modal,
                restore_focus: true,
                band: options.band,
                slot: options.slot,
                surface: OverlaySurface::menu(),
                scrim: None,
                a11y: OverlayA11y {
                    role: options.roles.panel,
                    label: options.label.clone(),
                },
                fade: true,
            },
        );
        let open = overlay.open_state();
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
            overlay,
            list,
            content,
            on_activate: RefCell::new(None),
            on_close: RefCell::new(None),
            state: RefCell::new(PanelState {
                window: None,
                surface: None,
                width: LogicalPx::ZERO,
                min_width: LogicalPx::ZERO,
                content_width: LogicalPx::ZERO,
                measured: false,
                measure: None,
                height: LogicalPx::ZERO,
                opened: false,
            }),
            open,
        });
        let panel = Self { inner };
        panel.register_actions(app);
        panel
    }

    pub(crate) fn overlay(&self) -> &Overlay {
        &self.inner.overlay
    }

    pub(crate) fn list(&self) -> &List {
        &self.inner.list
    }

    pub(crate) fn open_state(&self) -> Signal<bool> {
        self.inner.open
    }

    pub(crate) fn is_open(&self, app: &App) -> bool {
        self.inner.overlay.is_open(app)
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
        {
            let mut slot = self.inner.content.borrow_mut();
            *slot = Some(content);
        }
        self.inner.list.set_disabled_rows(disabled);
        self.inner.list.set_row_a11y(move |_cx, index| a11y(index));
        if self.inner.overlay.is_open(app) {
            self.inner.list.set_items(app, count)?;
            self.reset_measure(app)?;
            self.apply_geometry(app)?;
            self.select_first(app);
        }
        Ok(())
    }

    pub(crate) fn open(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        width: LogicalPx,
    ) -> PanelResult<bool> {
        let content = self.inner.content.borrow().clone();
        let Some(content) = content else {
            return Ok(false);
        };
        let width = (width.max(px(1.0)) - MENU_PADDING * 2.0).max(px(1.0));
        let total = content
            .rows
            .iter()
            .fold(LogicalPx::ZERO, |total, row| total + row.height);
        let height = total
            .min((self.inner.options.max_height - MENU_PADDING * 2.0).max(px(1.0)))
            .max(LogicalPx::ZERO);
        let key = self.inner.key.clone();
        let list = self.inner.list.clone();
        let activate = self.inner.actions.activate.clone();
        let text_size = self.inner.options.size.text_size();
        let measure = match self.inner.options.width {
            PanelWidth::Exact => None,
            PanelWidth::AtLeast => content.measure.clone(),
        };
        let surface_slot = Rc::new(Cell::new(None));
        let measure_slot = Rc::new(Cell::new(None));
        let slot = surface_slot.clone();
        let measure_slot_inner = measure_slot.clone();
        let opened = self
            .inner
            .overlay
            .open(app, window, anchor, move |tree, panel| {
                let surface = NodeBuilder::boxed()
                    .id(format_smolstr!("{key}:surface"))
                    .style(
                        StyleBuilder::new()
                            .flex_col()
                            .width(width)
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
                    .build(tree, panel)?;
                slot.set(Some(surface));
                list.build(tree, surface)?;
                if let Some(measure) = measure.as_ref()
                    && !measure.labels.is_empty()
                {
                    measure_slot_inner
                        .set(Some(build_measure_node(tree, surface, measure, text_size)?));
                }
                Ok(())
            })?;
        if !opened {
            self.resize(app, width)?;
            return Ok(false);
        }
        self.inner.list.attach(app, window)?;
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = Some(window);
            state.surface = surface_slot.get();
            state.min_width = width;
            state.content_width = LogicalPx::ZERO;
            state.width = width;
            state.measure = measure_slot.get();
            state.measured = state.measure.is_none();
            state.height = height;
            state.opened = true;
        }
        self.inner.list.set_items(app, content.rows.len())?;
        self.bind_keys(app, window)?;
        self.select_first(app);
        Ok(true)
    }

    pub(crate) fn resize(&self, app: &mut App, width: LogicalPx) -> Result<(), EngineError> {
        let min_width = (width.max(px(1.0)) - MENU_PADDING * 2.0).max(px(1.0));
        {
            let mut state = self.inner.state.borrow_mut();
            if state.surface.is_none() || (state.min_width - min_width).abs() < px(0.5) {
                return Ok(());
            }
            state.min_width = min_width;
        }
        self.write_surface_width(app)?;
        Ok(())
    }

    fn write_surface_width(&self, app: &mut App) -> Result<bool, EngineError> {
        let (window, surface, previous, target) = {
            let state = self.inner.state.borrow();
            let target = match self.inner.options.width {
                PanelWidth::Exact => state.min_width,
                PanelWidth::AtLeast => state.min_width.max(state.content_width),
            };
            (state.window, state.surface, state.width, target)
        };
        let (Some(window), Some(surface)) = (window, surface) else {
            return Ok(false);
        };
        if previous > LogicalPx::ZERO && (previous - target).abs() < px(0.5) {
            return Ok(false);
        }
        if let Some(tree) = app.window_mut(window).map(|target| target.tree_mut())
            && let Ok(node) = tree.get(surface)
        {
            let mut style = node.style().clone();
            style.width_length(target);
            tree.set_style(surface, style)?;
        }
        self.inner.state.borrow_mut().width = target;
        Ok(true)
    }

    fn reset_measure(&self, app: &mut App) -> Result<(), EngineError> {
        if self.inner.options.width != PanelWidth::AtLeast {
            return Ok(());
        }
        let (Some(window), Some(surface)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().surface,
        ) else {
            return Ok(());
        };
        let content = self.inner.content.borrow().clone();
        let text_size = self.inner.options.size.text_size();
        let mut state = self.inner.state.borrow_mut();
        state.content_width = LogicalPx::ZERO;
        state.measured = false;
        let Some(tree) = app.window_mut(window).map(|target| target.tree_mut()) else {
            return Ok(());
        };
        if let Some(previous) = state.measure.take() {
            let _ = tree.remove(previous);
        }
        let Some(measure) = content.and_then(|content| content.measure) else {
            state.measured = true;
            return Ok(());
        };
        if measure.labels.is_empty() {
            state.measured = true;
            return Ok(());
        }
        let node = build_measure_node(tree, surface, &measure, text_size)?;
        state.measure = Some(node);
        Ok(())
    }

    fn measure_content(&self, app: &App) -> Option<LogicalPx> {
        let (window, node) = {
            let state = self.inner.state.borrow();
            (state.window?, state.measure?)
        };
        let tree = app.window(window)?.tree();
        let content = self.inner.content.borrow();
        let measure = content.as_ref()?.measure.as_ref()?;
        let mut offset = 0usize;
        let mut width = LogicalPx::ZERO;
        for label in &measure.labels {
            let bytes = label.as_bytes();
            let mut segment_start = 0usize;
            for (index, byte) in bytes.iter().enumerate() {
                if *byte != b'\n' && *byte != b'\r' {
                    continue;
                }
                let start = tree.text_caret_rect(node, offset + segment_start).ok()?;
                let end = tree.text_caret_rect(node, offset + index).ok()?;
                width = width.max((end.origin.x - start.origin.x).abs());
                segment_start = index + 1;
            }
            let start = tree.text_caret_rect(node, offset + segment_start).ok()?;
            let end = tree.text_caret_rect(node, offset + label.len()).ok()?;
            width = width.max((end.origin.x - start.origin.x).abs());
            offset += label.len() + 1;
        }
        Some(width + measure.extra + MENU_PADDING * 2.0)
    }

    fn apply_content_width(&self, app: &mut App) -> Result<bool, EngineError> {
        if self.inner.options.width != PanelWidth::AtLeast {
            return Ok(false);
        }
        {
            let state = self.inner.state.borrow();
            if state.measured || state.measure.is_none() {
                return Ok(false);
            }
        }
        let Some(content_width) = self.measure_content(app) else {
            return Ok(false);
        };
        let (window, node) = {
            let mut state = self.inner.state.borrow_mut();
            state.content_width = content_width;
            state.measured = true;
            (state.window, state.measure.take())
        };
        if let (Some(window), Some(node)) = (window, node)
            && let Some(tree) = app.window_mut(window).map(|target| target.tree_mut())
        {
            let _ = tree.remove(node);
        }
        self.write_surface_width(app)
    }

    pub(crate) fn close(&self, app: &mut App) -> bool {
        self.inner.overlay.close(app)
    }

    pub(crate) fn close_now(&self, app: &mut App) -> bool {
        let closed = self.inner.overlay.close_now(app);
        self.finish_close(app);
        closed
    }

    pub(crate) fn on_activate(&self, callback: impl Fn(&mut App, usize) + 'static) {
        *self.inner.on_activate.borrow_mut() = Some(Rc::new(callback));
    }

    pub(crate) fn on_close(&self, callback: impl Fn(&mut App) + 'static) {
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

    pub(crate) fn sync(&self, app: &mut App, claim_focus: bool) -> Result<bool, EngineError> {
        let mut changed = self.inner.overlay.sync(app);
        if self.inner.state.borrow().opened && !self.inner.overlay.is_open(app) {
            self.finish_close(app);
            return Ok(true);
        }
        if !self.inner.overlay.is_open(app) {
            return Ok(changed);
        }
        changed |= self.inner.list.sync(app)?;
        changed |= self.apply_content_width(app)?;
        if claim_focus {
            changed |= self.claim_focus(app);
        }
        Ok(changed)
    }

    fn claim_focus(&self, app: &mut App) -> bool {
        let (Some(window), Some(surface)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().surface,
        ) else {
            return false;
        };
        let Some(root) = self.inner.overlay.overlay_node() else {
            return false;
        };
        if let Some(focused) = app.focused(window)
            && let Some(tree) = app.window(window).map(|target| target.tree())
        {
            let mut cursor = Some(focused);
            while let Some(node) = cursor {
                if node == root {
                    return false;
                }
                cursor = tree.get(node).ok().and_then(|target| target.parent());
            }
        }
        app.set_focus(window, Some(surface))
    }

    pub(crate) fn release(&self, app: &mut App) -> usize {
        self.close_now(app);
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
        self.inner.overlay.release(app);
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
            let Some(hit) = ctx.hit().map(|hit| hit.node) else {
                return;
            };
            let window = ctx.window();
            let app = ctx.app_mut();
            let Some(index) = this.row_at_node(app, window, hit) else {
                return;
            };
            this.focus(app);
            this.activate(app, index);
        });
    }

    fn bind_keys(&self, app: &mut App, window: WindowId) -> PanelResult<()> {
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

    fn apply_geometry(&self, app: &mut App) -> Result<(), EngineError> {
        let (window, surface, width) = {
            let state = self.inner.state.borrow();
            let width = match self.inner.options.width {
                PanelWidth::Exact => state.min_width,
                PanelWidth::AtLeast => state.min_width.max(state.content_width),
            };
            (state.window, state.surface, width)
        };
        let (Some(window), Some(surface)) = (window, surface) else {
            return Ok(());
        };
        let content = self.inner.content.borrow().clone();
        let Some(content) = content else {
            return Ok(());
        };
        let total = content
            .rows
            .iter()
            .fold(LogicalPx::ZERO, |total, row| total + row.height);
        let height = total
            .min((self.inner.options.max_height - MENU_PADDING * 2.0).max(px(1.0)))
            .max(LogicalPx::ZERO);
        {
            let mut state = self.inner.state.borrow_mut();
            state.height = height;
            state.width = width;
        }
        if let Some(tree) = app.window_mut(window).map(|target| target.tree_mut())
            && let Ok(node) = tree.get(surface)
        {
            let mut style = node.style().clone();
            style.size_length(width, height);
            tree.set_style(surface, style)?;
        }
        Ok(())
    }

    fn finish_close(&self, app: &mut App) {
        let was_opened = {
            let mut state = self.inner.state.borrow_mut();
            let opened = state.opened;
            state.opened = false;
            state.surface = None;
            state.window = None;
            state.measure = None;
            state.measured = false;
            state.content_width = LogicalPx::ZERO;
            state.width = LogicalPx::ZERO;
            state.min_width = LogicalPx::ZERO;
            state.height = LogicalPx::ZERO;
            opened
        };
        let _ = app.write(&self.inner.open, false);
        let _ = self.inner.list.clear_selection(app);
        if was_opened && let Some(callback) = self.inner.on_close.borrow().clone() {
            callback(app);
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

impl MenuEntry {
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        match self {
            Self::Action(action) => !action.disabled,
            Self::Submenu(submenu) => !submenu.disabled,
            Self::Group(_) | Self::Separator => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MenuMode {
    #[default]
    Menu,
    ListBox,
}

impl MenuMode {
    fn roles(self) -> PanelRoles {
        match self {
            Self::Menu => PanelRoles {
                panel: Some(A11yRole::Menu),
                row: A11yRole::MenuItem,
            },
            Self::ListBox => PanelRoles {
                panel: None,
                row: A11yRole::ListBoxOption,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MenuOptions {
    pub size: UnifiedSize,
    pub width: LogicalPx,
    pub max_height: LogicalPx,
    pub mode: MenuMode,
    pub placement: Placement,
    pub gap: LogicalPx,
    pub band: Band,
    pub label: Option<SmolStr>,
    pub submenu_delay: Duration,
}

impl Default for MenuOptions {
    fn default() -> Self {
        Self {
            size: UnifiedSize::Medium,
            width: px(200.0),
            max_height: px(320.0),
            mode: MenuMode::Menu,
            placement: Placement::BOTTOM_START,
            gap: px(4.0),
            band: Band::Popup,
            label: None,
            submenu_delay: SUBMENU_DELAY,
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
    pub fn mode(mut self, mode: MenuMode) -> Self {
        self.mode = mode;
        self
    }

    #[must_use]
    pub fn label(mut self, label: impl Into<SmolStr>) -> Self {
        self.label = Some(label.into());
        self
    }

    #[must_use]
    pub fn band(mut self, band: Band) -> Self {
        self.band = band;
        self
    }

    #[must_use]
    pub fn submenu_delay(mut self, delay: Duration) -> Self {
        self.submenu_delay = delay;
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
    on_close: RefCell<Option<Rc<dyn Fn(&mut App)>>>,
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
    layer: Layer,
    router: AnimationRouter,
    options: MenuOptions,
    kind: MenuKind,
    parent: Option<(Weak<MenuInner>, usize)>,
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
    layer: Layer,
    router: AnimationRouter,
    panel: ItemPanel,
    icons: RefCell<IconSnapshot>,
    rows: RefCell<Vec<RowKind>>,
    entries: RefCell<Rc<Vec<MenuEntry>>>,
    kind: MenuKind,
    parent: Option<(Weak<MenuInner>, usize)>,
    sink: Rc<MenuSink>,
    submenus: RefCell<Vec<OpenSubmenu>>,
    submenu_timer: Cell<Option<TimerId>>,
    last_hovered: Cell<Option<usize>>,
    window: Cell<Option<WindowId>>,
    right_action: Action,
    left_action: Action,
    claim_focus: Cell<bool>,
}

impl Menu {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
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
                layer,
                router,
                options,
                kind: MenuKind::Root,
                parent: None,
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
            layer,
            router,
            options,
            kind,
            parent,
            sink,
        } = seed;
        let key = key.into();
        let root = kind == MenuKind::Root;
        let panel = ItemPanel::new(
            app,
            theme.clone(),
            layer.clone(),
            router.clone(),
            format_smolstr!("{key}:panel"),
            ItemPanelOptions {
                size: options.size,
                max_height: options.max_height,
                row_height: options.size.h(),
                band: options.band,
                modal: root,
                placement: options.placement,
                gap: options.gap,
                roles: options.mode.roles(),
                label: options.label.clone(),
                width: PanelWidth::Exact,
                slot: if root {
                    LayerSlot::Exclusive
                } else {
                    LayerSlot::Nested
                },
            },
        );
        let inner = Rc::new(MenuInner {
            key: key.clone(),
            theme,
            options,
            layer,
            router,
            panel,
            icons: RefCell::new(IconSnapshot::default()),
            rows: RefCell::new(Vec::new()),
            entries: RefCell::new(Rc::new(entries.into_iter().collect())),
            kind,
            parent,
            sink,
            submenus: RefCell::new(Vec::new()),
            submenu_timer: Cell::new(None),
            last_hovered: Cell::new(None),
            window: Cell::new(None),
            right_action: Action::Custom(format_smolstr!("{key}.right")),
            left_action: Action::Custom(format_smolstr!("{key}.left")),
            claim_focus: Cell::new(false),
        });
        let menu = Self { inner };
        menu.install_content(app);
        menu.install_handlers();
        menu
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
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
        self.inner.panel.overlay().panel_node()
    }

    #[must_use]
    pub fn surface_node(&self) -> Option<NodeId> {
        self.inner.panel.surface_node()
    }

    pub(crate) fn overlay_node(&self) -> Option<NodeId> {
        self.inner.panel.overlay().overlay_node()
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
    ) -> Result<bool, OverlayError> {
        self.inner.window.set(Some(window));
        self.resolve_icons(app, window)?;
        self.install_content(app);
        let width = self.inner.options.width;
        let opened = self.inner.panel.open(app, window, anchor, width)?;
        if opened {
            self.inner.claim_focus.set(true);
            self.inner.panel.focus(app);
            self.bind_extra_keys(app, window)?;
        }
        Ok(opened)
    }

    pub fn open_at(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        highlighted: Option<&str>,
    ) -> Result<bool, OverlayError> {
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

    pub fn on_close(self, callback: impl Fn(&mut App) + 'static) -> Self {
        *self.inner.sink.on_close.borrow_mut() = Some(Rc::new(callback));
        self
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, EngineError> {
        let has_submenu = !self.inner.submenus.borrow().is_empty();
        let claim = self.inner.claim_focus.get() && !has_submenu;
        let mut changed = self.inner.panel.sync(app, claim)?;
        if !self.inner.panel.is_open(app) {
            self.close_submenus(app);
            return Ok(changed);
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
        self.close_now(app);
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
        let roles = self.inner.options.mode.roles();
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
                    role: Some(roles.row),
                    label: Some(action.label.clone()),
                    disabled: Some(action.disabled),
                    checked: action.checked,
                    selected: match (roles.row, action.checked) {
                        (A11yRole::ListBoxOption, Some(checked)) => Some(checked),
                        _ => None,
                    },
                    ..A11yPatch::default()
                },
                MenuEntry::Submenu(submenu) => A11yPatch {
                    role: Some(roles.row),
                    label: Some(submenu.label.clone()),
                    disabled: Some(submenu.disabled),
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
        self.inner.panel.on_close(move |app| {
            this.close_submenus(app);
            this.fire_close(app);
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

    fn fire_close(&self, app: &mut App) {
        if self.inner.kind != MenuKind::Root {
            return;
        }
        if let Some(callback) = self.inner.sink.on_close.borrow().clone() {
            callback(app);
        }
    }

    fn close_chain(&self, app: &mut App) {
        self.inner.panel.close_now(app);
        let parent = self
            .inner
            .parent
            .as_ref()
            .and_then(|(parent, _)| parent.upgrade());
        if let Some(parent) = parent {
            Menu { inner: parent }.close_chain(app);
        }
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
        options.placement = Placement::RIGHT_START;
        options.gap = SUBMENU_GAP;
        let child = Menu::build(
            app,
            format_smolstr!("{}:sub:{row}", self.inner.key),
            submenu.entries.clone(),
            MenuSeed {
                theme: self.inner.theme.clone(),
                layer: self.inner.layer.clone(),
                router: self.inner.router.clone(),
                options,
                kind: MenuKind::Submenu,
                parent: Some((Rc::downgrade(&self.inner), row)),
                sink: self.inner.sink.clone(),
            },
        );
        child.inner.claim_focus.set(focus);
        let anchor = Anchor::node(row_node);
        match child.open(app, window, anchor) {
            Ok(true) => {
                if focus {
                    let _ = child.inner.panel.focus(app);
                }
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
                if submenus[index].menu.is_open(app) {
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
                let delay = self.inner.options.submenu_delay;
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
                let _ = parent.inner.panel.focus(app);
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
        if self.inner.options.mode == MenuMode::Menu
            && entries
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

const MEASURE_WIDTH: LogicalPx = LogicalPx::new(4096.0);

fn build_measure_node(
    tree: &mut NodeTree,
    surface: NodeId,
    measure: &PanelMeasure,
    text_size: LogicalPx,
) -> Result<NodeId, EngineError> {
    let mut text = String::with_capacity(measure.labels.iter().map(|label| label.len() + 1).sum());
    for (index, label) in measure.labels.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(label);
    }
    NodeBuilder::text(
        SmolStr::new(text),
        TextStyle::new(text_size).with_wrap(TextWrap::NoWrap),
    )
    .style(
        StyleBuilder::new()
            .absolute()
            .left(-MEASURE_WIDTH)
            .top(px(0.0))
            .width(MEASURE_WIDTH)
            .height(px(1.0))
            .build(),
    )
    .a11y(A11yOverride {
        hidden: true,
        ..A11yOverride::default()
    })
    .build(tree, surface)
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
        a11y::A11yRole,
        event::{InputEvent, KeyCode, Modifiers, PointerButton},
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        test_support::app::TestApp,
        window::WindowId,
    };

    use super::{Menu, MenuAction, MenuEntry, MenuMode, MenuOptions, MenuSubmenu};
    use crate::{
        kit::{
            layer::Layer,
            overlay::{Anchor, AnimationRouter},
            test_support::{full_a11y, overlay_wrapper},
        },
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
        layer: Layer,
        router: AnimationRouter,
        anchor: NodeId,
        activated: Rc<RefCell<Vec<SmolStr>>>,
        closes: Rc<Cell<usize>>,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
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
            Self {
                app,
                window,
                theme,
                layer: Layer::new(),
                router: AnimationRouter::new(),
                anchor,
                activated: Rc::new(RefCell::new(Vec::new())),
                closes: Rc::new(Cell::new(0)),
            }
        }

        fn menu(&mut self, entries: Vec<MenuEntry>, options: MenuOptions) -> Menu {
            let activated = self.activated.clone();
            let closes = self.closes.clone();
            Menu::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                "menu",
                entries,
                options,
            )
            .on_activate(move |_, activation| activated.borrow_mut().push(activation.id))
            .on_close(move |_| closes.set(closes.get() + 1))
        }

        fn open(&mut self, menu: &Menu) -> bool {
            let opened = menu
                .open(self.app.app_mut(), self.window, Anchor::node(self.anchor))
                .expect("打开菜单");
            self.settle(menu);
            opened
        }

        fn settle(&mut self, menu: &Menu) {
            for _ in 0..6 {
                self.app.advance(Duration::from_millis(40));
                let _ = self.app.drain().expect("帧");
                let _ = menu.sync(self.app.app_mut());
            }
            self.app.advance(Duration::from_millis(200));
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
        }

        fn key(&mut self, menu: &Menu, code: KeyCode) {
            let _ = self
                .app
                .inject(InputEvent::key_down(code, Modifiers::empty()));
            let _ = self
                .app
                .inject(InputEvent::key_up(code, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
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
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
        }

        fn hover(&mut self, menu: &Menu, at: LogicalPoint) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(at, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
        }

        fn advance(&mut self, menu: &Menu, duration: Duration) {
            self.app.advance(duration);
            let _ = self.app.drain().expect("帧");
            let _ = menu.sync(self.app.app_mut());
        }

        fn bounds(&self, node: NodeId) -> LogicalRect {
            self.app.probe().bounds(node).expect("布局盒")
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

        fn root_children(&self) -> Vec<NodeId> {
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .get(
                    self.app
                        .app()
                        .window(self.window)
                        .expect("窗口")
                        .tree()
                        .root(),
                )
                .expect("根")
                .children()
                .to_vec()
        }

        fn idles(&mut self) -> bool {
            let record = self.app.drain().expect("帧");
            !record.rendered
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
        let opened = menu
            .open(harness.app.app_mut(), harness.window, Anchor::node(bottom))
            .expect("打开");
        assert!(opened);
        harness.settle(&menu);
        let panel = harness.bounds(menu.panel_node().expect("面板"));
        assert!(
            panel.max().y.value() <= 560.0 + 0.5,
            "放不下时翻到锚点上方：{panel:?}"
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
        assert_eq!(harness.closes.get(), 1, "关闭回调一次");
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
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn a_press_outside_closes_the_menu() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        harness.click(&menu, LogicalPoint::new(px(10.0), px(10.0)));
        assert!(!menu.is_open(harness.app.app()), "点击外部关闭");
        assert_eq!(harness.closes.get(), 1);
    }

    #[test]
    fn a_submenu_opens_beside_its_row_after_the_hover_delay() {
        let mut harness = Harness::new();
        let entries = vec![
            MenuEntry::Action(MenuAction::new("a", "A")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [MenuEntry::Action(MenuAction::new("inner", "内部"))],
            )),
        ];
        let menu = harness.menu(entries, options());
        harness.open(&menu);
        let root_before = harness.root_children().len();

        let at = harness.row_center(1);
        harness.hover(&menu, at);
        let root_during = harness.root_children().len();
        assert_eq!(root_during, root_before, "延迟未到不开子菜单");

        harness.advance(&menu, Duration::from_millis(200));
        let root_after = harness.root_children();
        assert_eq!(root_after.len(), root_before + 1, "延迟到点打开子菜单");
        let submenu_overlay = *root_after.last().expect("子菜单浮层");
        let submenu_panel = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .get(submenu_overlay)
            .expect("子菜单浮层")
            .children()
            .first()
            .copied()
            .expect("子菜单面板");
        let submenu = harness.bounds(submenu_panel);
        let row_bounds = harness.bounds(harness.row(1));
        assert!(
            submenu.origin.x >= row_bounds.max().x - 0.5,
            "子菜单在所属行右侧：行 {row_bounds:?} / 子 {submenu:?}"
        );
        assert!(
            (submenu.origin.y - row_bounds.origin.y).value().abs() < 1.0,
            "子菜单与所属行顶部对齐"
        );
    }

    #[test]
    fn escape_closes_the_submenu_before_the_menu_it_belongs_to() {
        let mut harness = Harness::new();
        let entries = vec![
            MenuEntry::Action(MenuAction::new("a", "A")),
            MenuEntry::Submenu(MenuSubmenu::new(
                "更多",
                [MenuEntry::Action(MenuAction::new("inner", "内部"))],
            )),
        ];
        let menu = harness.menu(entries, options());
        harness.open(&menu);
        let root_before = harness.root_children().len();
        harness.key(&menu, KeyCode::ArrowDown);
        harness.key(&menu, KeyCode::ArrowRight);
        assert_eq!(
            harness.root_children().len(),
            root_before + 1,
            "→ 打开子菜单"
        );

        harness.key(&menu, KeyCode::Escape);
        assert!(menu.is_open(harness.app.app()), "第一下 Esc 只关子菜单");
        assert_eq!(harness.root_children().len(), root_before);

        harness.key(&menu, KeyCode::Escape);
        assert!(!menu.is_open(harness.app.app()), "第二下 Esc 关父菜单");
        assert_eq!(harness.closes.get(), 1);
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
    fn the_menu_declares_menu_and_menu_item_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        let snapshot = full_a11y(&mut harness.app);

        let panel = menu.panel_node().expect("面板");
        let wrapper = overlay_wrapper(harness.app.app(), harness.window, panel);
        let wrapper_record = snapshot.node_for(wrapper).expect("包装节点在 a11y 树里");
        assert_eq!(wrapper_record.role, A11yRole::Menu);
        assert_eq!(
            snapshot.node_for(panel).expect("面板在 a11y 树里").role,
            A11yRole::GenericContainer,
            "面板节点不再重复声明 Menu（E12-R1）"
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
    fn a_settled_menu_with_no_input_requests_no_frames() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        assert!(
            !menu.sync(harness.app.app_mut()).expect("同步"),
            "稳态无工作"
        );
        assert!(harness.idles(), "稳态不出帧（I3）");
    }

    #[test]
    fn a_menu_opened_from_a_modal_window_keeps_the_scope_it_was_opened_in() {
        let mut harness = Harness::new();
        let menu = harness.menu(entries(), options());
        harness.open(&menu);
        assert!(
            harness.app.app().modal_scope(harness.window).is_some(),
            "菜单自带模态作用域"
        );
        harness.app.inject(InputEvent::pointer_move(
            LogicalPoint::new(px(5.0), px(5.0)),
            Modifiers::empty(),
        ));
        let _ = harness.app.drain().expect("帧");
        assert_eq!(
            harness.app.app().hovered(harness.window),
            None,
            "模态下的背景悬停被过滤（E12）"
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

    #[test]
    fn the_list_box_mode_declares_list_and_list_box_option_roles() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let menu = harness.menu(
            entries(),
            MenuOptions {
                mode: MenuMode::ListBox,
                ..options()
            },
        );
        harness.open(&menu);
        let snapshot = full_a11y(&mut harness.app);
        let panel = menu.panel_node().expect("面板");
        let wrapper = overlay_wrapper(harness.app.app(), harness.window, panel);
        assert_eq!(
            snapshot.node_for(wrapper).expect("包装").role,
            A11yRole::GenericContainer,
            "ListBox 角色缺位（E15）时包装不声明角色"
        );
        assert_eq!(
            snapshot
                .nodes()
                .iter()
                .filter(|node| node.role == A11yRole::List)
                .count(),
            1,
            "集合语义只来自内层 E9 虚拟列表容器（不得双 List）"
        );
        assert!(
            snapshot
                .nodes()
                .iter()
                .any(|node| node.role == A11yRole::ListBoxOption),
            "行声明 ListBoxOption"
        );
    }
}
