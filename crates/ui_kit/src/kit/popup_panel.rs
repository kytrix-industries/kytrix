use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::A11yOverride,
    app::App,
    error::EngineError,
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{
        color::Color,
        style::{ShadowSpec, Shadows, Style, StylePatch},
        style_builder::{PaintBuilder, StyleBuilder},
    },
    popup::{
        Anchor, DismissOutcome, DismissReason, PopupA11y, PopupDismiss, PopupError, PopupId,
        PopupSpec, PopupState,
    },
    text::{TextStyle, TextWrap},
    window::WindowId,
};

use crate::theme::{components::ComponentTokens, handle::ThemeHandle, role::Role};

const MEASURE_WIDTH: LogicalPx = LogicalPx::new(4096.0);

#[derive(Clone, Debug, PartialEq)]
pub struct PopupSurface {
    pub background: Role,
    pub corner_radius: LogicalPx,
    pub padding: LogicalPx,
    pub shadows: Shadows,
}

impl PopupSurface {
    #[must_use]
    pub fn menu() -> Self {
        let tokens = ComponentTokens::standard();
        let color = Color::BLACK.with_opacity(tokens.shadow.menu_alpha);
        Self {
            background: Role::SurfaceOverlay,
            corner_radius: px(4.0),
            padding: px(4.0),
            shadows: Shadows::new([
                ShadowSpec {
                    offset: LogicalPoint::new(px(0.0), px(1.0)),
                    blur: px(2.0),
                    spread: px(0.0),
                    color,
                },
                ShadowSpec {
                    offset: LogicalPoint::new(px(0.0), px(3.0)),
                    blur: px(6.0),
                    spread: px(-2.0),
                    color,
                },
            ]),
        }
    }

    #[must_use]
    pub fn dialog() -> Self {
        let tokens = ComponentTokens::standard();
        Self {
            background: Role::SurfaceOverlay,
            corner_radius: px(4.0),
            padding: px(8.0),
            shadows: Shadows::from(ShadowSpec {
                offset: LogicalPoint::new(px(0.0), px(25.0)),
                blur: px(50.0),
                spread: px(-12.0),
                color: Color::BLACK.with_opacity(tokens.shadow.dialog_alpha),
            }),
        }
    }

    #[must_use]
    pub fn tooltip() -> Self {
        Self {
            background: Role::SurfaceInverse,
            corner_radius: px(4.0),
            padding: px(4.0),
            shadows: Shadows::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AnchorSample {
    rect: Option<LogicalRect>,
    viewport: Option<LogicalSize>,
    scroll_epoch: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AnchorChange {
    #[default]
    None,
    Lost,
    Resized,
    Scrolled,
    Moved,
}

#[derive(Default)]
pub(crate) struct AnchorWatch {
    previous: Option<AnchorSample>,
}

impl AnchorWatch {
    pub(crate) fn reset(&mut self) {
        self.previous = None;
    }

    pub(crate) fn observe(&mut self, app: &App, id: PopupId, window: WindowId) -> AnchorChange {
        let current = AnchorSample {
            rect: app.popup_anchor_rect(id),
            viewport: app.window(window).map(|target| target.size()),
            scroll_epoch: app.window_scroll_epoch(window),
        };
        let Some(previous) = self.previous.replace(current) else {
            return AnchorChange::None;
        };
        let point_anchor = current.rect.is_some_and(|rect| {
            rect.size.width <= LogicalPx::ZERO || rect.size.height <= LogicalPx::ZERO
        });
        if !point_anchor && app.popup_anchor_visible(id) == Some(false) {
            return AnchorChange::Lost;
        }
        if let (Some(before), Some(after)) = (previous.rect, current.rect)
            && before != after
        {
            return AnchorChange::Moved;
        }
        if previous.viewport.is_some() && previous.viewport != current.viewport {
            return AnchorChange::Resized;
        }
        if previous.scroll_epoch.is_some() && previous.scroll_epoch != current.scroll_epoch {
            return AnchorChange::Scrolled;
        }
        AnchorChange::None
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PanelWidth {
    Auto,
    Exact(LogicalPx),
    AtLeast { min: LogicalPx },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelMeasure {
    pub labels: Vec<SmolStr>,
    pub extra: LogicalPx,
    pub text_size: LogicalPx,
}

impl PanelMeasure {
    #[must_use]
    pub fn new(labels: Vec<SmolStr>, extra: LogicalPx) -> Self {
        Self {
            labels,
            extra,
            text_size: ComponentTokens::standard().small.font_size,
        }
    }

    #[must_use]
    pub fn text_size(mut self, text_size: LogicalPx) -> Self {
        self.text_size = text_size;
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PopupPanelOptions {
    pub surface: PopupSurface,
    pub width: PanelWidth,
    pub measure: Option<PanelMeasure>,
}

impl PopupPanelOptions {
    #[must_use]
    pub fn new(surface: PopupSurface, width: PanelWidth) -> Self {
        Self {
            surface,
            width,
            measure: None,
        }
    }

    #[must_use]
    pub fn measure(mut self, measure: PanelMeasure) -> Self {
        self.measure = Some(measure);
        self
    }
}

#[derive(Default)]
struct PanelState {
    window: Option<WindowId>,
    id: Option<PopupId>,
    content: Option<NodeId>,
    panel: Option<NodeId>,
    body: Option<NodeId>,
    measure_node: Option<NodeId>,
    content_width: LogicalPx,
    margin: LogicalPx,
}

struct PanelInner {
    key: SmolStr,
    theme: ThemeHandle,
    surface: PopupSurface,
    width: RefCell<PanelWidth>,
    measure: RefCell<Option<PanelMeasure>>,
    scope: SignalScope,
    open: Signal<bool>,
    reason: Cell<Option<DismissReason>>,
    on_close: RefCell<Option<Rc<dyn Fn(&mut App, DismissReason)>>>,
    on_dismiss: RefCell<Option<Rc<dyn Fn(&mut App, &PopupDismiss) -> DismissOutcome>>>,
    state: RefCell<PanelState>,
}

#[derive(Clone)]
pub struct PopupPanel {
    inner: Rc<PanelInner>,
}

impl PopupPanel {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        key: impl Into<SmolStr>,
        options: PopupPanelOptions,
    ) -> Self {
        let scope = app.signal_scope();
        let open = app.signal_scoped(scope, false);
        Self {
            inner: Rc::new(PanelInner {
                key: key.into(),
                theme,
                surface: options.surface,
                width: RefCell::new(options.width),
                measure: RefCell::new(options.measure),
                scope,
                open,
                reason: Cell::new(None),
                on_close: RefCell::new(None),
                on_dismiss: RefCell::new(None),
                state: RefCell::new(PanelState::default()),
            }),
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.open
    }

    pub fn on_close(&self, callback: impl Fn(&mut App, DismissReason) + 'static) {
        *self.inner.on_close.borrow_mut() = Some(Rc::new(callback));
    }

    pub(crate) fn on_dismiss(
        &self,
        callback: impl Fn(&mut App, &PopupDismiss) -> DismissOutcome + 'static,
    ) {
        *self.inner.on_dismiss.borrow_mut() = Some(Rc::new(callback));
    }

    #[must_use]
    pub fn id(&self) -> Option<PopupId> {
        self.inner.state.borrow().id
    }

    #[must_use]
    pub fn state(&self, app: &App) -> Option<PopupState> {
        app.popup_state(self.id()?)
    }

    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.state(app).is_some_and(PopupState::is_dismissable)
    }

    #[must_use]
    pub fn is_closing(&self, app: &App) -> bool {
        self.state(app).is_some_and(|state| {
            matches!(
                state,
                PopupState::DismissPending | PopupState::Closing | PopupState::Closed
            )
        })
    }

    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().panel
    }

    #[must_use]
    pub fn body_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().body
    }

    #[must_use]
    pub(crate) fn available_body_height(&self, app: &App) -> Option<LogicalPx> {
        let (window, id) = {
            let state = self.inner.state.borrow();
            (state.window?, state.id?)
        };
        let stack = app.popup_stack(window)?;
        let entry = stack.entries.iter().find(|entry| entry.id == id)?;
        if entry.hit_rect.size.height <= LogicalPx::ZERO {
            return None;
        }
        let height = entry
            .constrained_size
            .map_or(entry.hit_rect.size.height, |size| size.height);
        Some((height - self.inner.surface.padding * 2.0).max(px(1.0)))
    }

    pub(crate) fn available_body_width(&self, app: &App) -> Option<LogicalPx> {
        let (window, id) = {
            let state = self.inner.state.borrow();
            (state.window?, state.id?)
        };
        let stack = app.popup_stack(window)?;
        let entry = stack.entries.iter().find(|entry| entry.id == id)?;
        if entry.hit_rect.size.width <= LogicalPx::ZERO {
            return None;
        }
        let width = entry
            .constrained_size
            .map_or(entry.hit_rect.size.width, |size| size.width);
        Some((width - self.inner.surface.padding * 2.0).max(px(1.0)))
    }

    #[must_use]
    pub fn popup_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().content
    }

    pub fn open(
        &self,
        app: &mut App,
        window: WindowId,
        spec: PopupSpec,
        content: impl FnOnce(&mut App, WindowId, NodeId) -> Result<(), EngineError>,
    ) -> Result<PopupId, PopupError> {
        let existing = self.inner.state.borrow().id;
        if let Some(existing) = existing {
            match app.popup_state(existing) {
                Some(state) if state.is_dismissable() => {
                    app.set_popup_anchor(existing, spec.anchor)?;
                    app.raise_popup(existing)?;
                    let _ = app.write(&self.inner.open, true);
                    return Ok(existing);
                }
                Some(PopupState::Closed) | None => {
                    self.finish_close(app);
                }
                Some(_) => {
                    app.close_popup(existing)?;
                    self.finish_close(app);
                }
            }
        }
        let key = self.inner.key.clone();
        let theme = self.inner.theme.clone();
        let surface = self.inner.surface.clone();
        let width = *self.inner.width.borrow();
        let measure = self.inner.measure.borrow().clone();
        let margin = spec.placement.viewport_margin;
        let built: Rc<RefCell<Option<BuiltPanel>>> = Rc::new(RefCell::new(None));
        let built_slot = Rc::clone(&built);
        let mut content = Some(content);
        let weak = Rc::downgrade(&self.inner);
        let id = app.open_popup(
            window,
            spec,
            move |app, window, node| {
                let (panel, body, measure_node) = {
                    let Some(target) = app.window_mut(window) else {
                        return Err(EngineError::UnknownWindow { window });
                    };
                    let tree = target.tree_mut();
                    let (panel, body) =
                        build_panel_nodes(tree, node, &key, &theme, &surface, width)?;
                    let measure_node = match (&measure, width) {
                        (Some(measure), PanelWidth::AtLeast { .. }) => {
                            Some(build_measure_node(tree, body, measure, &key)?)
                        }
                        _ => None,
                    };
                    (panel, body, measure_node)
                };
                if let Some(content) = content.take() {
                    content(app, window, body)?;
                }
                built_slot.borrow_mut().replace(BuiltPanel {
                    content: node,
                    panel,
                    body,
                    measure_node,
                });
                Ok(())
            },
            Some(Box::new(move |app, dismiss| {
                let Some(inner) = weak.upgrade() else {
                    return DismissOutcome::Close;
                };
                let callback = inner.on_dismiss.borrow().clone();
                let outcome = match callback {
                    Some(callback) => callback(app, &dismiss),
                    None => DismissOutcome::Close,
                };
                if outcome != DismissOutcome::Ignore {
                    inner.reason.set(Some(dismiss.reason));
                }
                outcome
            })),
        )?;
        let Some(built) = built.borrow_mut().take() else {
            let _ = app.close_popup(id);
            return Err(PopupError::UnknownPopup { id });
        };
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = Some(window);
            state.id = Some(id);
            state.content = Some(built.content);
            state.panel = Some(built.panel);
            state.body = Some(built.body);
            state.measure_node = built.measure_node;
            state.content_width = LogicalPx::ZERO;
            state.margin = margin;
        }
        self.inner.reason.set(None);
        let _ = app.write(&self.inner.open, true);
        let _ = self.apply_measure(app);
        Ok(id)
    }

    pub fn close(&self, app: &mut App) -> bool {
        self.close_with_reason(app, DismissReason::Programmatic)
    }

    pub fn close_with_reason(&self, app: &mut App, reason: DismissReason) -> bool {
        let Some(id) = self.inner.state.borrow().id else {
            return false;
        };
        if self.is_closing(app) {
            return true;
        }
        let closing = app.begin_close_popup(id, reason).unwrap_or(false);
        if closing {
            self.inner.reason.set(Some(reason));
            let _ = app.write(&self.inner.open, false);
        }
        closing
    }

    pub fn close_now(&self, app: &mut App) -> bool {
        let Some(id) = self.inner.state.borrow().id else {
            return false;
        };
        let closed = if app.popup_state(id).is_some() {
            app.close_popup(id).is_ok()
        } else {
            false
        };
        self.finish_close(app) || closed
    }

    pub fn set_anchor(&self, app: &mut App, anchor: Anchor) -> bool {
        let Some(id) = self.inner.state.borrow().id else {
            return false;
        };
        app.set_popup_anchor(id, anchor).is_ok()
    }

    pub fn request_measure(&self, app: &mut App) -> bool {
        let Some(id) = self.inner.state.borrow().id else {
            return false;
        };
        let mut changed = app.request_popup_measure(id).is_ok();
        changed |= self.apply_measure(app);
        changed
    }

    pub fn set_measure(&self, app: &mut App, measure: Option<PanelMeasure>) -> bool {
        if *self.inner.measure.borrow() == measure {
            return false;
        }
        *self.inner.measure.borrow_mut() = measure;
        if self.inner.state.borrow().id.is_none() {
            return true;
        }
        self.rebuild_measure_node(app);
        self.apply_measure(app);
        true
    }

    pub fn set_width(&self, app: &mut App, width: PanelWidth) -> bool {
        if *self.inner.width.borrow() == width {
            return false;
        }
        *self.inner.width.borrow_mut() = width;
        if self.inner.state.borrow().id.is_none() {
            return true;
        }
        self.apply_width(app)
    }

    pub fn set_a11y(&self, app: &mut App, a11y: PopupA11y) -> bool {
        let Some(id) = self.inner.state.borrow().id else {
            return false;
        };
        app.set_popup_a11y(id, a11y).unwrap_or(false)
    }

    pub fn sync(&self, app: &mut App) -> bool {
        let Some(id) = self.inner.state.borrow().id else {
            return false;
        };
        let mut changed = false;
        match app.popup_state(id) {
            None | Some(PopupState::Closed) => {
                changed |= self.finish_close(app);
            }
            Some(PopupState::DismissPending | PopupState::Closing) => {
                if *app.read(&self.inner.open) {
                    let _ = app.write(&self.inner.open, false);
                    changed = true;
                }
            }
            Some(PopupState::Opening | PopupState::Open) => {
                if !*app.read(&self.inner.open) {
                    let _ = app.write(&self.inner.open, true);
                    changed = true;
                }
                changed |= self.apply_measure(app);
            }
        }
        changed
    }

    pub fn release(&self, app: &mut App) {
        self.inner.on_close.borrow_mut().take();
        self.inner.on_dismiss.borrow_mut().take();
        self.close_now(app);
        let _ = app.release_scope(self.inner.scope);
    }

    fn finish_close(&self, app: &mut App) -> bool {
        let (reason, was_open) = {
            let mut state = self.inner.state.borrow_mut();
            let was_open = state.id.take().is_some();
            state.window = None;
            state.content = None;
            state.panel = None;
            state.body = None;
            state.measure_node = None;
            state.content_width = LogicalPx::ZERO;
            (
                self.inner
                    .reason
                    .take()
                    .unwrap_or(DismissReason::Programmatic),
                was_open,
            )
        };
        if !was_open {
            return false;
        }
        let _ = app.write(&self.inner.open, false);
        if let Some(callback) = self.inner.on_close.borrow().clone() {
            callback(app, reason);
        }
        true
    }

    fn rebuild_measure_node(&self, app: &mut App) {
        let (Some(window), Some(body)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().body,
        ) else {
            return;
        };
        let measure = self.inner.measure.borrow().clone();
        let previous = self.inner.state.borrow_mut().measure_node.take();
        let Some(tree) = app.window_mut(window).map(|target| target.tree_mut()) else {
            return;
        };
        if let Some(previous) = previous {
            let _ = tree.remove(previous);
        }
        let node = measure
            .and_then(|measure| build_measure_node(tree, body, &measure, &self.inner.key).ok());
        self.inner.state.borrow_mut().measure_node = node;
    }

    fn apply_measure(&self, app: &mut App) -> bool {
        let PanelWidth::AtLeast { min } = *self.inner.width.borrow() else {
            return false;
        };
        let (window, measure_node, panel_node, id, margin) = {
            let state = self.inner.state.borrow();
            let (Some(window), Some(measure_node), Some(panel_node), Some(id)) =
                (state.window, state.measure_node, state.panel, state.id)
            else {
                return false;
            };
            (window, measure_node, panel_node, id, state.margin)
        };
        let Some(measure) = self.inner.measure.borrow().clone() else {
            return false;
        };
        if measure.labels.is_empty() {
            return false;
        }
        if let Some(Err(_)) = app.refresh_text(window) {
            return false;
        }
        let Some(target) = app.window_mut(window) else {
            return false;
        };
        if target.compute_layout().is_err() {
            return false;
        }
        if let Some(Err(_)) = app.layout_text(window) {
            return false;
        }
        let Some(target) = app.window_mut(window) else {
            return false;
        };
        if target.compute_layout().is_err() {
            return false;
        }
        let Some(target) = app.window(window) else {
            return false;
        };
        let viewport = target.size();
        let Some(content_width) = measure_content_width(
            target.tree(),
            measure_node,
            &measure,
            self.inner.surface.padding,
        ) else {
            return false;
        };
        let available = px((viewport.width.value() - margin.value() * 2.0).max(0.0));
        let target_width = content_width.max(min).min(available);
        {
            let Some(tree) = app.window_mut(window).map(|target| target.tree_mut()) else {
                return false;
            };
            let Ok(node) = tree.get(panel_node) else {
                return false;
            };
            let mut style = node.style().clone();
            style.width_length(target_width);
            if tree.set_style(panel_node, style).is_err() {
                return false;
            }
            if let Some(measure_node) = self.inner.state.borrow_mut().measure_node.take() {
                let _ = tree.remove(measure_node);
            }
        }
        self.inner.state.borrow_mut().content_width = content_width;
        let _ = app.request_popup_measure(id);
        true
    }

    fn apply_width(&self, app: &mut App) -> bool {
        let width = *self.inner.width.borrow();
        let (Some(window), Some(panel_node), Some(id), margin) = ({
            let state = self.inner.state.borrow();
            (state.window, state.panel, state.id, state.margin)
        }) else {
            return false;
        };
        let content_width = self.inner.state.borrow().content_width;
        let Some(target) = app.window(window) else {
            return false;
        };
        let available = px((target.size().width.value() - margin.value() * 2.0).max(0.0));
        let Some(tree) = app.window_mut(window).map(|target| target.tree_mut()) else {
            return false;
        };
        let Ok(node) = tree.get(panel_node) else {
            return false;
        };
        let mut style = node.style().clone();
        match width {
            PanelWidth::Auto => style.width(taffy::style::Dimension::auto()),
            PanelWidth::Exact(width) => style.width_length(width),
            PanelWidth::AtLeast { min } => {
                style.width_length(content_width.max(min).min(available))
            }
        };
        if tree.set_style(panel_node, style).is_err() {
            return false;
        }
        let _ = app.request_popup_measure(id);
        true
    }
}

struct BuiltPanel {
    content: NodeId,
    panel: NodeId,
    body: NodeId,
    measure_node: Option<NodeId>,
}

fn panel_style(surface: &PopupSurface, width: PanelWidth) -> Style {
    let mut style = StyleBuilder::new()
        .flex_col()
        .min_width_0()
        .min_height_0()
        .padding(surface.padding)
        .overflow_hidden();
    match width {
        PanelWidth::Auto => {}
        PanelWidth::Exact(width) => style = style.width(width),
        PanelWidth::AtLeast { min } => style = style.width(min),
    }
    style.build()
}

fn build_panel_nodes(
    tree: &mut NodeTree,
    parent: NodeId,
    key: &str,
    theme: &ThemeHandle,
    surface: &PopupSurface,
    width: PanelWidth,
) -> Result<(NodeId, NodeId), EngineError> {
    let mut paint = PaintBuilder::new().rounded(surface.corner_radius).build();
    paint.shadows = surface.shadows.clone();
    let background = surface.background;
    let theme = theme.clone();
    let panel = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:panel"))
        .style(panel_style(surface, width))
        .paint(paint)
        .bind_style(move |cx| StylePatch {
            background: Some(theme.role(cx, background)),
            ..StylePatch::default()
        })
        .build(tree, parent)?;
    let body = NodeBuilder::boxed()
        .id(format_smolstr!("{key}:body"))
        .style(
            StyleBuilder::new()
                .flex_col()
                .min_width_0()
                .min_height_0()
                .overflow_hidden()
                .build(),
        )
        .build(tree, panel)?;
    Ok((panel, body))
}

fn build_measure_node(
    tree: &mut NodeTree,
    parent: NodeId,
    measure: &PanelMeasure,
    key: &str,
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
        TextStyle::new(measure.text_size).with_wrap(TextWrap::NoWrap),
    )
    .id(format_smolstr!("{key}:measure"))
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
    .build(tree, parent)
}

fn measure_content_width(
    tree: &NodeTree,
    node: NodeId,
    measure: &PanelMeasure,
    padding: LogicalPx,
) -> Option<LogicalPx> {
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
    Some(width + measure.extra + padding * 2.0)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc, time::Duration};

    use smol_str::SmolStr;
    use taffy::style::Overflow;
    use ui_core::{
        a11y::{A11yRole, A11yTreeSnapshot},
        error::EngineError,
        event::{InputEvent, KeyCode, Modifiers},
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::builder::NodeBuilder,
        paint::{primitive::Primitive, style_builder::StyleBuilder},
        popup::{
            Anchor, DismissOutcome, DismissReason, FocusPolicy, PlacementSpec, PopupA11y,
            PopupAnimation, PopupId, PopupSpec,
        },
        test_support::{app::TestApp, record::FrameRecord},
        window::WindowId,
    };

    use super::{PanelMeasure, PanelWidth, PopupPanel, PopupPanelOptions, PopupSurface};
    use crate::theme::{appearance::Appearance, handle::ThemeHandle};

    const TRIGGER: LogicalRect = LogicalRect::from_origin_size(
        LogicalPoint::new(px(300.0), px(200.0)),
        LogicalSize::new(px(100.0), px(40.0)),
    );

    const CONTENT: LogicalSize = LogicalSize::new(px(100.0), px(40.0));

    struct Harness {
        app: TestApp,
        window: WindowId,
        trigger: ui_core::node::id::NodeId,
        panel: PopupPanel,
        reasons: Rc<RefCell<Vec<DismissReason>>>,
    }

    impl Harness {
        fn new(width: PanelWidth) -> Self {
            Self::with_measure(width, None)
        }

        fn with_measure(width: PanelWidth, measure: Option<PanelMeasure>) -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            let root = app.app().window(window).expect("窗口").tree().root();
            let trigger = {
                let tree = app.app_mut().window_mut(window).expect("窗口").tree_mut();
                NodeBuilder::boxed()
                    .id("popup-trigger")
                    .layout_override(TRIGGER)
                    .build(tree, root)
                    .expect("触发器")
            };
            let mut options = PopupPanelOptions::new(PopupSurface::menu(), width);
            if let Some(measure) = measure {
                options = options.measure(measure);
            }
            let panel = PopupPanel::new(app.app_mut(), theme, "gallery.menu:panel", options);
            let reasons = Rc::new(RefCell::new(Vec::new()));
            let recorded = Rc::clone(&reasons);
            panel.on_close(move |_app, reason| recorded.borrow_mut().push(reason));
            let _ = app.drain().expect("预热帧");
            Self {
                app,
                window,
                trigger,
                panel,
                reasons,
            }
        }

        fn spec(&self) -> PopupSpec {
            let mut spec = PopupSpec::new(
                Anchor::Node { node: self.trigger },
                PlacementSpec::default(),
            );
            spec.focus = FocusPolicy::None;
            spec
        }

        fn open(&mut self) -> PopupId {
            let spec = self.spec();
            self.panel
                .open(
                    self.app.app_mut(),
                    self.window,
                    spec,
                    |app, window, body| {
                        let Some(target) = app.window_mut(window) else {
                            return Err(EngineError::UnknownWindow { window });
                        };
                        NodeBuilder::boxed()
                            .id("popup-content-box")
                            .style(
                                StyleBuilder::new()
                                    .size(CONTENT.width, CONTENT.height)
                                    .build(),
                            )
                            .build(target.tree_mut(), body)
                            .map(|_| ())
                    },
                )
                .expect("打开面板")
        }

        fn drain(&mut self) -> FrameRecord {
            self.app.drain().expect("帧").clone()
        }

        fn advance(&mut self, by: Duration) -> FrameRecord {
            self.app.advance(by);
            self.drain()
        }

        fn rect(&self, id: PopupId) -> LogicalRect {
            self.app.app().popup_rect(id).expect("浮层 rect").hit
        }

        fn panel_rect(&self) -> LogicalRect {
            let node = self.panel.panel_node().expect("面板节点");
            self.app.probe().bounds(node).expect("面板布局")
        }

        fn hit(&self, point: LogicalPoint) -> Option<ui_core::node::id::NodeId> {
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .hit_test(point)
                .map(|hit| hit.node)
        }

        fn hits_popup(&self, point: LogicalPoint) -> bool {
            let Some(hit) = self.hit(point) else {
                return false;
            };
            let Some(content) = self.panel.popup_node() else {
                return false;
            };
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            let mut cursor = Some(hit);
            while let Some(node) = cursor {
                if node == content {
                    return true;
                }
                cursor = tree.get(node).ok().and_then(|target| target.parent());
            }
            false
        }

        fn center_of(&self, rect: LogicalRect) -> LogicalPoint {
            LogicalPoint::new(
                rect.origin.x + rect.size.width * 0.5,
                rect.origin.y + rect.size.height * 0.5,
            )
        }

        fn a11y(&mut self) -> A11yTreeSnapshot {
            self.app.enable_a11y_capture();
            let _ = self.app.drain().expect("a11y 帧");
            self.app.a11y().expect("a11y 快照").clone()
        }
    }

    #[test]
    fn the_first_visible_frame_already_has_the_settled_rect() {
        let mut harness = Harness::new(PanelWidth::Exact(px(180.0)));
        let id = harness.open();
        let first_frame = harness.drain();
        let first = harness.rect(id);
        assert!(first_frame.rendered, "打开当帧必须绘制");
        assert_eq!(
            harness.panel.state(harness.app.app()),
            Some(ui_core::popup::PopupState::Open),
            "首帧即 Open（不再需要等一帧才 reveal）"
        );
        assert!(
            (first.size.width - 180.0).value().abs() < 0.5,
            "Exact 宽度立即生效，实际 {first:?}"
        );
        for _ in 0..5 {
            harness.drain();
        }
        assert_eq!(first, harness.rect(id), "首可见帧 rect 必须等于稳定帧");
        assert_eq!(first, harness.panel_rect(), "popup rect 必须等于面板布局盒");
    }

    #[test]
    fn the_panel_shadow_is_not_clipped_by_the_popup_content_root() {
        let mut harness = Harness::new(PanelWidth::Exact(px(180.0)));
        let _ = harness.open();
        harness.advance(Duration::from_millis(200));
        let panel = harness.panel.panel_node().expect("面板节点");
        let content = harness.panel.popup_node().expect("浮层内容根");
        let panel_rect = harness.panel_rect();
        let window = harness.app.app().window(harness.window).expect("窗口");
        let content_overflow = window
            .tree()
            .get(content)
            .expect("内容根节点")
            .style()
            .inner()
            .overflow;
        assert_eq!(
            (content_overflow.x, content_overflow.y),
            (Overflow::Visible, Overflow::Visible),
            "popup 内容根不得裁剪，阴影必须能外溢"
        );
        let lane = harness.app.probe().lane_of(panel);
        let window = harness.app.app().window(harness.window).expect("窗口");
        let shadow = lane
            .and_then(|lane| window.scene().lane_primitives(lane))
            .and_then(|primitives| {
                primitives.iter().find_map(|primitive| match primitive {
                    Primitive::Shadow(shadow) => Some(*shadow),
                    _ => None,
                })
            })
            .expect("面板阴影图元必须被绘制");
        assert!(
            shadow.blur > px(0.0) || shadow.offset != LogicalPoint::ZERO,
            "阴影必须带模糊或偏移"
        );
        assert_eq!(shadow.rect, panel_rect, "阴影底矩形覆盖面板边界");
    }

    #[test]
    fn an_at_least_panel_widens_to_the_measured_content() {
        let min = px(120.0);
        let mut narrow = Harness::with_measure(
            PanelWidth::AtLeast { min },
            Some(PanelMeasure::new(vec![SmolStr::new_static("短")], px(0.0))),
        );
        let narrow_id = narrow.open();
        narrow.drain();
        let narrow_rect = narrow.rect(narrow_id);
        assert!(
            (narrow_rect.size.width - 120.0).value().abs() < 0.5,
            "内容比最小宽度窄时保持 min，实际 {narrow_rect:?}"
        );

        let mut wide = Harness::with_measure(
            PanelWidth::AtLeast { min },
            Some(PanelMeasure::new(
                vec![SmolStr::new_static(
                    "一段足够长的菜单项文本用来撑开面板宽度",
                )],
                px(0.0),
            )),
        );
        let wide_id = wide.open();
        wide.drain();
        let wide_rect = wide.rect(wide_id);
        assert!(
            wide_rect.size.width > narrow_rect.size.width + 20.0,
            "AtLeast 必须按内容自然宽回写面板宽度：{wide_rect:?} vs {narrow_rect:?}"
        );
        assert!(
            wide_rect.size.width <= px(800.0),
            "宽度不得超出视口减 margin，实际 {wide_rect:?}"
        );
        for _ in 0..4 {
            wide.drain();
        }
        assert_eq!(wide_rect, wide.rect(wide_id), "测量回写后首帧即稳定");
    }

    #[test]
    fn an_at_least_panel_without_measure_keeps_its_minimum_width() {
        let min = px(150.0);
        let mut harness = Harness::new(PanelWidth::AtLeast { min });
        let id = harness.open();
        harness.drain();
        let rect = harness.rect(id);
        assert!(
            (rect.size.width - 150.0).value().abs() < 0.5,
            "没有测量数据时至少保持 min 宽度，实际 {rect:?}"
        );
    }

    #[test]
    fn updating_the_measure_widens_an_open_panel() {
        let min = px(120.0);
        let mut harness = Harness::with_measure(
            PanelWidth::AtLeast { min },
            Some(PanelMeasure::new(vec![SmolStr::new_static("短")], px(0.0))),
        );
        let id = harness.open();
        harness.drain();
        let before = harness.rect(id);
        assert!(
            harness.panel.set_measure(
                harness.app.app_mut(),
                Some(PanelMeasure::new(
                    vec![SmolStr::new_static("内容变化后更长的菜单项文本")],
                    px(0.0),
                )),
            ),
            "更新测量数据必须被感知"
        );
        harness.drain();
        let after = harness.rect(id);
        assert!(
            after.size.width > before.size.width + 20.0,
            "打开后更新测量必须回写宽度：{after:?} vs {before:?}"
        );
    }

    #[test]
    fn changing_the_width_after_open_takes_effect_in_the_same_frame() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        let before = harness.rect(id);
        assert!(
            harness
                .panel
                .set_width(harness.app.app_mut(), PanelWidth::Exact(px(240.0))),
            "宽度变化必须被感知"
        );
        let record = harness.drain();
        let after = harness.rect(id);
        assert!(record.rendered, "宽度变化当帧必须重绘");
        assert!(
            (after.size.width - 240.0).value().abs() < 0.5,
            "新宽度当帧生效，实际 {after:?}"
        );
        assert!(after.size.width > before.size.width, "宽度确实变宽");
        harness.drain();
        assert_eq!(after, harness.rect(id), "改宽后首帧即稳定");
        assert_eq!(after, harness.panel_rect(), "popup rect 跟随面板布局盒");
    }

    #[test]
    fn an_at_least_panel_honours_minimum_width_changes() {
        let min = px(120.0);
        let mut harness = Harness::new(PanelWidth::AtLeast { min });
        let id = harness.open();
        harness.drain();
        let narrow = harness.rect(id);
        assert!(
            (narrow.size.width - 120.0).value().abs() < 0.5,
            "没有测量数据时保持 AtLeast 下限 120，实际 {narrow:?}"
        );
        assert!(
            harness.panel.set_width(
                harness.app.app_mut(),
                PanelWidth::AtLeast { min: px(220.0) }
            ),
            "下限提高必须被感知"
        );
        harness.drain();
        let wide = harness.rect(id);
        assert!(
            (wide.size.width - 220.0).value().abs() < 0.5,
            "AtLeast 下限提高后面板变宽，实际 {wide:?}"
        );
        assert!(harness.panel_rect().size.width >= wide.size.width - 0.5);
    }

    #[test]
    fn setting_the_same_width_is_a_no_op() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.advance(Duration::from_millis(200));
        let before = harness.rect(id);
        assert!(
            !harness
                .panel
                .set_width(harness.app.app_mut(), PanelWidth::Exact(px(160.0))),
            "相同宽度必须返回 false"
        );
        let record = harness.drain();
        assert!(!record.rendered, "相同宽度不得触发重排/重绘");
        assert_eq!(before, harness.rect(id));
    }

    #[test]
    fn opening_and_closing_a_popup_never_forces_a_full_repaint() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        harness.drain();
        let id = harness.open();
        for _ in 0..4 {
            let record = harness.drain();
            assert!(!record.damage_full, "打开浮层不得全量重绘");
        }
        assert!(harness.panel.close(harness.app.app_mut()));
        for _ in 0..12 {
            let record = harness.advance(Duration::from_millis(16));
            assert!(!record.damage_full, "关闭浮层不得全量重绘");
            if harness.panel.state(harness.app.app()).is_none() {
                break;
            }
        }
        assert!(
            harness.app.app().popup_rect(id).is_none(),
            "关闭动画结束后节点卸载"
        );
        let record = harness.drain();
        assert!(!record.damage_full, "卸载帧不得全量重绘");
    }

    #[test]
    fn a_closing_panel_is_no_longer_hittable() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        let center = harness.center_of(harness.rect(id));
        assert!(harness.hits_popup(center), "打开时面板可命中");
        assert!(harness.panel.close(harness.app.app_mut()));
        assert!(harness.panel.is_closing(harness.app.app()));
        assert!(
            !harness.panel.is_open(harness.app.app()),
            "关闭一开始 is_open 即为 false（修 K6 toggle 吞点击）"
        );
        assert!(
            !*harness.app.app().read(&harness.panel.open_state()),
            "关闭开始时 open 信号同步为 false（expanded 同帧清除）"
        );
        assert!(
            !harness.hits_popup(center),
            "Closing/DismissPending 层不可命中"
        );
    }

    #[test]
    fn opening_an_open_panel_keeps_the_same_popup() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let first = harness.open();
        harness.drain();
        let second = harness.open();
        assert_eq!(first, second, "已打开时 open 幂等并沿用同一 PopupId");
        harness.drain();
        let stack = harness.app.app().popup_stack(harness.window).expect("栈");
        assert_eq!(stack.entries.len(), 1, "不得叠出第二个面板");
    }

    #[test]
    fn the_close_callback_reports_the_dismiss_reason_exactly_once() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
        harness.drain();
        assert!(
            harness.panel.is_closing(harness.app.app()),
            "Esc 进入关闭事务"
        );
        for _ in 0..20 {
            harness.advance(Duration::from_millis(16));
            let _ = harness.panel.sync(harness.app.app_mut());
            if harness.app.app().popup_rect(id).is_none() {
                break;
            }
        }
        assert_eq!(
            harness.reasons.borrow().as_slice(),
            &[DismissReason::Escape],
            "on_close 必须收到引擎 dismiss 原因且只回调一次"
        );
        let _ = harness.panel.sync(harness.app.app_mut());
        let _ = harness.panel.sync(harness.app.app_mut());
        assert_eq!(harness.reasons.borrow().len(), 1, "on_close 不得重复回调");
    }

    #[test]
    fn closing_programmatically_reports_a_programmatic_reason() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        assert!(harness.panel.close(harness.app.app_mut()));
        for _ in 0..20 {
            harness.advance(Duration::from_millis(16));
            let _ = harness.panel.sync(harness.app.app_mut());
            if harness.app.app().popup_rect(id).is_none() {
                break;
            }
        }
        assert_eq!(
            harness.reasons.borrow().as_slice(),
            &[DismissReason::Programmatic]
        );
    }

    #[test]
    fn closing_with_an_explicit_reason_reports_that_reason() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        assert!(
            harness
                .panel
                .close_with_reason(harness.app.app_mut(), DismissReason::AnchorLost),
            "带原因的关闭必须启动关闭事务"
        );
        assert!(harness.panel.is_closing(harness.app.app()));
        for _ in 0..20 {
            harness.advance(Duration::from_millis(16));
            let _ = harness.panel.sync(harness.app.app_mut());
            if harness.app.app().popup_rect(id).is_none() {
                break;
            }
        }
        assert_eq!(
            harness.reasons.borrow().as_slice(),
            &[DismissReason::AnchorLost],
            "on_close 必须收到调用方显式给出的原因"
        );
        assert!(
            !harness
                .panel
                .close_with_reason(harness.app.app_mut(), DismissReason::AnchorLost),
            "已关闭的面板不再启动关闭"
        );
        assert_eq!(harness.reasons.borrow().len(), 1, "on_close 不得重复回调");
    }

    #[test]
    fn a_refused_close_does_not_leak_its_reason_into_a_later_close() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        harness.panel.on_dismiss(|_, _| DismissOutcome::Ignore);
        assert!(
            !harness
                .panel
                .close_with_reason(harness.app.app_mut(), DismissReason::AnchorLost),
            "dismiss handler 拒绝时不得进入关闭事务"
        );
        assert!(!harness.panel.is_closing(harness.app.app()));
        assert_eq!(
            harness.panel.state(harness.app.app()),
            Some(ui_core::popup::PopupState::Open),
            "拒绝后保持打开"
        );
        assert!(harness.app.app().popup_rect(id).is_some(), "拒绝后浮层仍在");
        assert!(harness.panel.close_now(harness.app.app_mut()));
        assert_eq!(
            harness.reasons.borrow().as_slice(),
            &[DismissReason::Programmatic],
            "失败关闭尝试的 AnchorLost 不得泄漏到后续非 dispatch 关闭（P3E-08）"
        );
        assert_eq!(harness.reasons.borrow().len(), 1);
    }

    #[test]
    fn release_does_not_report_a_close_callback() {
        let mut harness = Harness::new(PanelWidth::Exact(px(180.0)));
        let id = harness.open();
        harness.drain();
        harness.panel.release(harness.app.app_mut());
        assert!(
            harness.reasons.borrow().is_empty(),
            "release 是静默拆卸，不得触发 on_close（K1-13）"
        );
        assert!(
            harness.app.app().popup_rect(id).is_none(),
            "release 必须卸载浮层"
        );
        assert!(!harness.panel.is_open(harness.app.app()));
    }

    #[test]
    fn close_now_unloads_immediately_and_reports_once() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        assert!(harness.panel.close_now(harness.app.app_mut()));
        assert!(
            harness.app.app().popup_rect(id).is_none(),
            "close_now 立即卸载（无淡出）"
        );
        assert_eq!(harness.reasons.borrow().len(), 1);
        assert!(
            !harness.panel.close_now(harness.app.app_mut()),
            "重复 close_now 是空操作"
        );
    }

    #[test]
    fn setting_the_popup_a11y_updates_the_engine_projection() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        let node = harness.panel.popup_node().expect("浮层内容节点");
        let mut a11y = PopupA11y {
            role: Some(A11yRole::Tooltip),
            ..PopupA11y::default()
        };
        a11y.label = Some(SmolStr::new_static("提示文本"));
        assert!(harness.panel.set_a11y(harness.app.app_mut(), a11y));
        let _ = id;
        let snapshot = harness.a11y();
        let record = snapshot.node_for(node).expect("浮层 a11y 记录");
        assert_eq!(record.role, A11yRole::Tooltip);
        assert_eq!(record.label_text(), Some("提示文本"));
    }

    #[test]
    fn set_anchor_repositions_the_panel() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let id = harness.open();
        harness.drain();
        let before = harness.rect(id);
        let target = LogicalPoint::new(px(40.0), px(20.0));
        assert!(harness.panel.set_anchor(
            harness.app.app_mut(),
            Anchor::WindowRect(LogicalRect::from_origin_size(
                target,
                LogicalSize::new(px(1.0), px(1.0))
            )),
        ));
        harness.drain();
        let after = harness.rect(id);
        assert!(
            after.origin != before.origin,
            "set_anchor 必须重新 settle：{before:?} -> {after:?}"
        );
        assert!(
            (after.origin.y - (target.y + px(5.0))).value().abs() < 0.5,
            "Bottom 方向 gap=4：面板顶边 = 锚点底边 + gap，实际 {after:?}"
        );
    }

    #[test]
    fn a_panel_without_animation_closes_in_the_same_frame() {
        let mut harness = Harness::new(PanelWidth::Exact(px(160.0)));
        let mut spec = harness.spec();
        spec.animation = PopupAnimation::None;
        let id = harness
            .panel
            .open(
                harness.app.app_mut(),
                harness.window,
                spec,
                |app, window, body| {
                    let Some(target) = app.window_mut(window) else {
                        return Err(EngineError::UnknownWindow { window });
                    };
                    NodeBuilder::boxed()
                        .style(
                            StyleBuilder::new()
                                .size(CONTENT.width, CONTENT.height)
                                .build(),
                        )
                        .build(target.tree_mut(), body)
                        .map(|_| ())
                },
            )
            .expect("打开面板");
        harness.drain();
        assert!(harness.panel.close(harness.app.app_mut()));
        assert!(
            harness.app.app().popup_rect(id).is_none(),
            "animation=None 时关闭立即卸载"
        );
    }
}
