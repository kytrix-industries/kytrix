use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
};

use smol_str::{SmolStr, format_smolstr};
use ui_core::{
    a11y::{A11yOverride, A11yRole},
    app::{AnimationCompletion, AnimationEndKind, App},
    error::EngineError,
    interaction::{
        AnimatedValue, AnimationId, AnimationSpec, Easing, Overlay as EngineOverlay,
        OverlayDismiss, OverlayOptions as EngineOverlayOptions, OverlayPlacement,
    },
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope, Transient},
        tree::NodeTree,
    },
    paint::{
        color::Color,
        style::{ShadowSpec, Shadows, Style, StylePatch},
        style_builder::{PaintBuilder, StyleBuilder},
    },
    window::{FrameIndex, WindowId},
};

use crate::{
    kit::layer::{Band, Layer, LayerSlot},
    theme::{components::ComponentTokens, handle::ThemeHandle, role::Role},
};

#[derive(Clone, Default)]
pub struct AnimationRouter {
    inner: Rc<RefCell<AnimationRouterState>>,
}

#[derive(Default)]
struct AnimationRouterState {
    installed: bool,
    consumers: Vec<(AnimationId, Weak<OverlayInner>)>,
}

impl AnimationRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn installed(&self) -> bool {
        self.inner.borrow().installed
    }

    fn install(&self, app: &mut App) {
        if self.inner.borrow().installed {
            return;
        }
        self.inner.borrow_mut().installed = true;
        let router = self.clone();
        app.on_animation_complete(move |app, completion| router.dispatch(app, completion));
    }

    fn track(&self, id: AnimationId, overlay: &Rc<OverlayInner>) {
        let mut state = self.inner.borrow_mut();
        state.consumers.retain(|(tracked, _)| *tracked != id);
        state.consumers.push((id, Rc::downgrade(overlay)));
    }

    fn untrack(&self, id: Option<AnimationId>) {
        let Some(id) = id else {
            return;
        };
        self.inner
            .borrow_mut()
            .consumers
            .retain(|(tracked, _)| *tracked != id);
    }

    fn dispatch(&self, app: &mut App, completion: AnimationCompletion) {
        let consumer = {
            let mut state = self.inner.borrow_mut();
            state
                .consumers
                .iter()
                .position(|(tracked, _)| *tracked == completion.id)
                .map(|index| state.consumers.swap_remove(index).1)
        };
        if let Some(inner) = consumer.and_then(|weak| weak.upgrade()) {
            Overlay { inner }.animation_completed(app, completion);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

impl Side {
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Alignment {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    pub side: Side,
    pub align: Alignment,
}

impl Placement {
    #[must_use]
    pub const fn new(side: Side, align: Alignment) -> Self {
        Self { side, align }
    }

    pub const TOP_START: Self = Self::new(Side::Top, Alignment::Start);
    pub const TOP: Self = Self::new(Side::Top, Alignment::Center);
    pub const TOP_END: Self = Self::new(Side::Top, Alignment::End);
    pub const BOTTOM_START: Self = Self::new(Side::Bottom, Alignment::Start);
    pub const BOTTOM: Self = Self::new(Side::Bottom, Alignment::Center);
    pub const BOTTOM_END: Self = Self::new(Side::Bottom, Alignment::End);
    pub const LEFT_START: Self = Self::new(Side::Left, Alignment::Start);
    pub const LEFT: Self = Self::new(Side::Left, Alignment::Center);
    pub const LEFT_END: Self = Self::new(Side::Left, Alignment::End);
    pub const RIGHT_START: Self = Self::new(Side::Right, Alignment::Start);
    pub const RIGHT: Self = Self::new(Side::Right, Alignment::Center);
    pub const RIGHT_END: Self = Self::new(Side::Right, Alignment::End);
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacementConfig {
    pub gap: LogicalPx,
    pub margin: LogicalPx,
    pub flip: bool,
}

impl Default for PlacementConfig {
    fn default() -> Self {
        Self {
            gap: px(4.0),
            margin: px(4.0),
            flip: true,
        }
    }
}

impl PlacementConfig {
    fn validate(self) -> Result<(), OverlayError> {
        if self.gap.is_finite() && self.margin.is_finite() {
            Ok(())
        } else {
            Err(OverlayError::PlacementConfig {
                gap: self.gap,
                margin: self.margin,
            })
        }
    }
}

#[derive(Debug)]
pub enum OverlayError {
    PlacementConfig { gap: LogicalPx, margin: LogicalPx },
    Engine(EngineError),
}

impl From<EngineError> for OverlayError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl fmt::Display for OverlayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlacementConfig { gap, margin } => write!(
                formatter,
                "invalid overlay placement config: gap {gap} / margin {margin} must be finite"
            ),
            Self::Engine(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OverlayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::PlacementConfig { .. } => None,
            Self::Engine(error) => Some(error),
        }
    }
}

#[must_use]
pub fn resolve_origin(
    anchor: LogicalRect,
    panel: LogicalSize,
    viewport: LogicalSize,
    placement: Placement,
    config: PlacementConfig,
) -> LogicalPoint {
    if !anchor.is_finite()
        || !panel.width.is_finite()
        || !panel.height.is_finite()
        || !viewport.width.is_finite()
        || !viewport.height.is_finite()
        || !config.gap.is_finite()
        || !config.margin.is_finite()
    {
        return LogicalPoint::ZERO;
    }
    let gap = config.gap.max(LogicalPx::ZERO);
    let margin = config.margin.max(LogicalPx::ZERO);
    let mut side = placement.side;
    if config.flip && !main_axis_fits(anchor, panel, side, viewport, gap, margin) {
        let opposite = side.opposite();
        if main_axis_fits(anchor, panel, opposite, viewport, gap, margin)
            || available(anchor, opposite, viewport, gap, margin)
                > available(anchor, side, viewport, gap, margin)
        {
            side = opposite;
        }
    }
    let placed = candidate(anchor, panel, Placement::new(side, placement.align), gap);
    LogicalPoint::new(
        clamp_axis(
            placed.origin.x,
            margin,
            viewport.width - margin - panel.width,
        ),
        clamp_axis(
            placed.origin.y,
            margin,
            viewport.height - margin - panel.height,
        ),
    )
}

fn candidate(
    anchor: LogicalRect,
    panel: LogicalSize,
    placement: Placement,
    gap: LogicalPx,
) -> LogicalRect {
    let (x, y) = match placement.side {
        Side::Bottom => (
            aligned_start(
                anchor.origin.x,
                anchor.size.width,
                panel.width,
                placement.align,
            ),
            anchor.max().y + gap,
        ),
        Side::Top => (
            aligned_start(
                anchor.origin.x,
                anchor.size.width,
                panel.width,
                placement.align,
            ),
            anchor.origin.y - gap - panel.height,
        ),
        Side::Right => (
            anchor.max().x + gap,
            aligned_start(
                anchor.origin.y,
                anchor.size.height,
                panel.height,
                placement.align,
            ),
        ),
        Side::Left => (
            anchor.origin.x - gap - panel.width,
            aligned_start(
                anchor.origin.y,
                anchor.size.height,
                panel.height,
                placement.align,
            ),
        ),
    };
    LogicalRect::new(x, y, panel.width, panel.height)
}

fn aligned_start(
    anchor_origin: LogicalPx,
    anchor_size: LogicalPx,
    panel_size: LogicalPx,
    align: Alignment,
) -> LogicalPx {
    match align {
        Alignment::Start => anchor_origin,
        Alignment::Center => anchor_origin + (anchor_size - panel_size) * 0.5,
        Alignment::End => anchor_origin + anchor_size - panel_size,
    }
}

fn main_axis_fits(
    anchor: LogicalRect,
    panel: LogicalSize,
    side: Side,
    viewport: LogicalSize,
    gap: LogicalPx,
    margin: LogicalPx,
) -> bool {
    const TOLERANCE: LogicalPx = LogicalPx::new(0.5);
    match side {
        Side::Bottom => anchor.max().y + gap + panel.height <= viewport.height - margin + TOLERANCE,
        Side::Top => anchor.origin.y - gap - panel.height >= margin - TOLERANCE,
        Side::Right => anchor.max().x + gap + panel.width <= viewport.width - margin + TOLERANCE,
        Side::Left => anchor.origin.x - gap - panel.width >= margin - TOLERANCE,
    }
}

fn available(
    anchor: LogicalRect,
    side: Side,
    viewport: LogicalSize,
    gap: LogicalPx,
    margin: LogicalPx,
) -> LogicalPx {
    match side {
        Side::Bottom => viewport.height - margin - anchor.max().y - gap,
        Side::Top => anchor.origin.y - margin - gap,
        Side::Right => viewport.width - margin - anchor.max().x - gap,
        Side::Left => anchor.origin.x - margin - gap,
    }
}

fn clamp_axis(value: LogicalPx, min: LogicalPx, max: LogicalPx) -> LogicalPx {
    if max < min {
        min
    } else {
        value.clamp(min, max)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    Node(NodeId),
    Fixed(LogicalRect),
}

impl Anchor {
    #[must_use]
    pub const fn node(node: NodeId) -> Self {
        Self::Node(node)
    }

    #[must_use]
    pub const fn point(point: LogicalPoint) -> Self {
        Self::Fixed(LogicalRect::from_origin_size(point, LogicalSize::ZERO))
    }

    #[must_use]
    pub const fn rect(rect: LogicalRect) -> Self {
        Self::Fixed(rect)
    }
}

#[must_use]
pub fn anchor_rect(app: &App, window: WindowId, anchor: Anchor) -> Option<LogicalRect> {
    match anchor {
        Anchor::Fixed(rect) => Some(rect),
        Anchor::Node(node) => {
            let tree = app.window(window)?.tree();
            let bounds = tree.get(node).ok()?.layout().bounds;
            let mut offset = LogicalPoint::ZERO;
            let mut cursor = tree.get(node).ok()?.parent();
            while let Some(current) = cursor {
                if let Some(handle) = app.scroll_handle(window, current)
                    && let Some(state) = app.scroll_state(&handle)
                {
                    offset =
                        LogicalPoint::new(offset.x + state.offset.x, offset.y + state.offset.y);
                }
                cursor = tree.get(current).ok()?.parent();
            }
            Some(bounds.translate(LogicalPoint::new(-offset.x, -offset.y)))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DismissPolicy {
    OutsideClick,
    Explicit,
}

impl DismissPolicy {
    fn engine(self) -> OverlayDismiss {
        match self {
            Self::OutsideClick => OverlayDismiss::OutsideClick,
            Self::Explicit => OverlayDismiss::Explicit,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlaySurface {
    pub background: Role,
    pub corner_radius: LogicalPx,
    pub padding: LogicalPx,
    pub shadows: Shadows,
}

impl OverlaySurface {
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
pub struct Scrim {
    pub density: f32,
}

impl Default for Scrim {
    fn default() -> Self {
        Self { density: 1.0 }
    }
}

impl Scrim {
    #[must_use]
    pub const fn new(density: f32) -> Self {
        Self { density }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlayA11y {
    pub role: Option<A11yRole>,
    pub label: Option<SmolStr>,
}

impl OverlayA11y {
    #[must_use]
    pub fn dialog(label: impl Into<SmolStr>) -> Self {
        Self {
            role: Some(A11yRole::Dialog),
            label: Some(label.into()),
        }
    }

    #[must_use]
    pub fn alert_dialog(label: impl Into<SmolStr>) -> Self {
        Self {
            role: Some(A11yRole::AlertDialog),
            label: Some(label.into()),
        }
    }

    #[must_use]
    pub fn menu(label: impl Into<SmolStr>) -> Self {
        Self {
            role: Some(A11yRole::Menu),
            label: Some(label.into()),
        }
    }

    #[must_use]
    pub fn tooltip(label: impl Into<SmolStr>) -> Self {
        Self {
            role: Some(A11yRole::Tooltip),
            label: Some(label.into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OverlayOptions {
    pub placement: Placement,
    pub gap: LogicalPx,
    pub margin: LogicalPx,
    pub flip: bool,
    pub dismiss: DismissPolicy,
    pub modal: bool,
    pub restore_focus: bool,
    pub band: Band,
    pub slot: LayerSlot,
    pub surface: OverlaySurface,
    pub scrim: Option<Scrim>,
    pub a11y: OverlayA11y,
    pub fade: bool,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            placement: Placement::BOTTOM_START,
            gap: px(4.0),
            margin: px(4.0),
            flip: true,
            dismiss: DismissPolicy::OutsideClick,
            modal: false,
            restore_focus: true,
            band: Band::Popup,
            slot: LayerSlot::Exclusive,
            surface: OverlaySurface::menu(),
            scrim: None,
            a11y: OverlayA11y::default(),
            fade: true,
        }
    }
}

impl OverlayOptions {
    fn placement_config(&self) -> PlacementConfig {
        PlacementConfig {
            gap: self.gap,
            margin: self.margin,
            flip: self.flip,
        }
    }
}

#[derive(Default)]
struct OverlayState {
    window: Option<WindowId>,
    handle: Option<EngineOverlay>,
    node: Option<NodeId>,
    group: Option<NodeId>,
    panel: Option<NodeId>,
    scrim: Option<NodeId>,
    anchor: Option<Anchor>,
    origin: Option<LogicalPoint>,
    frame_at_open: Option<FrameIndex>,
    scrim_geometry: Option<(LogicalPoint, LogicalSize)>,
    closing: bool,
}

struct OverlayInner {
    key: SmolStr,
    theme: ThemeHandle,
    layer: Layer,
    router: AnimationRouter,
    options: OverlayOptions,
    scope: SignalScope,
    open: Signal<bool>,
    revealed: Signal<bool>,
    fade: Transient<f32>,
    fade_animation: Cell<Option<AnimationId>>,
    a11y: RefCell<OverlayA11y>,
    state: RefCell<OverlayState>,
}

#[derive(Clone)]
pub struct Overlay {
    inner: Rc<OverlayInner>,
}

impl Overlay {
    pub fn new(
        app: &mut App,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
        options: OverlayOptions,
    ) -> Self {
        let key = key.into();
        let scope = app.signal_scope();
        router.install(app);
        let a11y = options.a11y.clone();
        Self {
            inner: Rc::new(OverlayInner {
                open: app.signal_scoped(scope, false),
                revealed: app.signal_scoped(scope, false),
                fade: app.transient_scoped(scope, 0.0),
                key,
                theme,
                layer,
                router,
                options,
                scope,
                fade_animation: Cell::new(None),
                a11y: RefCell::new(a11y),
                state: RefCell::new(OverlayState::default()),
            }),
        }
    }

    #[must_use]
    pub fn key(&self) -> &str {
        &self.inner.key
    }

    #[must_use]
    pub fn options(&self) -> &OverlayOptions {
        &self.inner.options
    }

    pub(crate) fn set_a11y(&self, a11y: OverlayA11y) {
        *self.inner.a11y.borrow_mut() = a11y;
    }

    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.open
    }

    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        *app.read(&self.inner.open)
    }

    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().panel
    }

    #[must_use]
    pub fn overlay_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().node
    }

    #[must_use]
    pub fn scrim_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().scrim
    }

    #[must_use]
    pub fn progress(&self, app: &App) -> f32 {
        *app.read_transient(&self.inner.fade)
    }

    pub fn open(
        &self,
        app: &mut App,
        window: WindowId,
        anchor: Anchor,
        content: impl FnOnce(&mut NodeTree, NodeId) -> Result<(), EngineError>,
    ) -> Result<bool, OverlayError> {
        self.inner.options.placement_config().validate()?;
        if app.window(window).is_none() {
            return Err(EngineError::UnknownWindow { window }.into());
        }
        let existing = self.inner.state.borrow().handle;
        if let Some(handle) = existing {
            if handle.window() == window {
                if self.is_alive(app, window) {
                    let (group, was_closing) = {
                        let mut state = self.inner.state.borrow_mut();
                        state.anchor = Some(anchor);
                        let was_closing = std::mem::take(&mut state.closing);
                        (state.group, was_closing)
                    };
                    if was_closing && let Some(group) = group {
                        self.start_fade(app, window, group, 1.0);
                    }
                    self.place(app);
                    return Ok(false);
                }
                self.finish_close(app);
            } else {
                self.close_now(app);
            }
        }
        let viewport = app
            .window(window)
            .map_or(LogicalSize::ZERO, |target| target.size());
        let Some(anchor_rect) = anchor_rect(app, window, anchor) else {
            return Ok(false);
        };
        let initial_origin = resolve_origin(
            anchor_rect,
            LogicalSize::ZERO,
            viewport,
            self.inner.options.placement,
            self.inner.options.placement_config(),
        );
        let a11y = self.inner.a11y.borrow().clone();
        let engine_overlay = app.create_overlay(
            window,
            EngineOverlayOptions {
                placement: OverlayPlacement::Point(initial_origin),
                dismiss: self.inner.options.dismiss.engine(),
                trap_focus: self.inner.options.modal,
                restore_focus: self.inner.options.restore_focus,
                a11y: A11yOverride {
                    role: a11y.role,
                    label: a11y.label,
                    ..A11yOverride::default()
                },
            },
        )?;
        let engine_node = engine_overlay.node();

        let built = {
            let tree = app.window_mut(window).expect("窗口刚检查过").tree_mut();
            let group = self.build_group(tree, engine_node)?;
            let scrim = if self.inner.options.modal
                && let Some(scrim) = self.inner.options.scrim
            {
                Some(self.build_scrim(tree, group, viewport, initial_origin, scrim)?)
            } else {
                None
            };
            let panel = self.build_panel(tree, group)?;
            match content(tree, panel) {
                Ok(()) => Ok((group, panel, scrim)),
                Err(error) => Err(error),
            }
        };
        let (group, panel, scrim) = match built {
            Ok(built) => built,
            Err(error) => {
                let _ = app.close_overlay(&engine_overlay);
                return Err(error.into());
            }
        };
        let frame_at_open = app.window(window).map(|target| target.frame_index());
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = Some(window);
            state.handle = Some(engine_overlay);
            state.node = Some(engine_node);
            state.group = Some(group);
            state.panel = Some(panel);
            state.scrim = scrim;
            state.anchor = Some(anchor);
            state.origin = Some(initial_origin);
            state.frame_at_open = frame_at_open;
            state.scrim_geometry = scrim.map(|_| (initial_origin, viewport));
            state.closing = false;
        }
        self.inner.layer.register_with(
            app,
            self.inner.options.band,
            engine_overlay,
            self.inner.options.modal,
            self.inner.options.slot,
        );
        let _ = app.write(&self.inner.open, true);
        self.start_fade(app, window, group, 1.0);
        let _ = self.sync_scrim(app, window, initial_origin, viewport);
        let _ = self.inner.layer.enforce(app, window);
        Ok(true)
    }

    pub fn place(&self, app: &mut App) -> bool {
        let (window, handle, node, anchor) = {
            let state = self.inner.state.borrow();
            let (Some(window), Some(handle), Some(node), Some(anchor)) =
                (state.window, state.handle, state.node, state.anchor)
            else {
                return false;
            };
            (window, handle, node, anchor)
        };
        let Some(anchor_rect) = anchor_rect(app, window, anchor) else {
            self.close_now(app);
            return true;
        };
        let Some(viewport) = app.window(window).map(|target| target.size()) else {
            return false;
        };
        let Some(panel_size) = app
            .window(window)
            .and_then(|target| target.tree().get(node).ok())
            .map(|target| target.layout().bounds.size)
        else {
            return false;
        };
        let origin = resolve_origin(
            anchor_rect,
            panel_size,
            viewport,
            self.inner.options.placement,
            self.inner.options.placement_config(),
        );
        let mut changed = false;
        let frame_at_open = self.inner.state.borrow().frame_at_open;
        let measured = frame_at_open.is_none_or(|opened| {
            app.window(window)
                .is_some_and(|target| target.frame_index() != opened)
        });
        if !*app.read(&self.inner.revealed) && measured {
            let _ = app.write(&self.inner.revealed, true);
            if !self.inner.options.fade {
                app.write_transient(&self.inner.fade, 1.0);
            }
            changed = true;
        }
        let previous = self.inner.state.borrow().origin;
        let moved = previous.is_none_or(|previous| {
            (previous.x - origin.x).abs().value() >= 0.5
                || (previous.y - origin.y).abs().value() >= 0.5
        });
        if moved {
            changed |= app.place_overlay(&handle, OverlayPlacement::Point(origin));
            self.inner.state.borrow_mut().origin = Some(origin);
        }
        changed |= self.sync_scrim(app, window, origin, viewport);
        changed
    }

    pub fn sync(&self, app: &mut App) -> bool {
        let (window, handle) = {
            let state = self.inner.state.borrow();
            let (Some(window), Some(handle)) = (state.window, state.handle) else {
                return false;
            };
            (window, handle)
        };
        let alive = app
            .window(window)
            .is_some_and(|target| target.tree().get(handle.node()).is_ok());
        if !alive {
            self.finish_close(app);
            return true;
        }
        if self.inner.state.borrow().closing {
            return false;
        }
        let mut changed = self.inner.layer.enforce(app, window);
        changed |= self.place(app);
        changed
    }

    pub fn close(&self, app: &mut App) -> bool {
        if self.inner.state.borrow().handle.is_none() {
            return false;
        }
        if self.inner.state.borrow().closing {
            return true;
        }
        if !self.inner.options.fade || self.progress(app) <= 0.0 {
            return self.close_now(app);
        }
        self.inner.state.borrow_mut().closing = true;
        let (Some(window), Some(group)) = (
            self.inner.state.borrow().window,
            self.inner.state.borrow().group,
        ) else {
            return self.close_now(app);
        };
        self.start_fade(app, window, group, 0.0);
        true
    }

    pub fn close_now(&self, app: &mut App) -> bool {
        let Some(handle) = self.inner.state.borrow().handle else {
            return false;
        };
        self.cancel_fade(app);
        let closed = app.close_overlay(&handle);
        self.finish_close(app);
        closed
    }

    pub fn release(&self, app: &mut App) {
        self.close_now(app);
        let _ = app.release_scope(self.inner.scope);
    }

    fn finish_close(&self, app: &mut App) {
        self.cancel_fade(app);
        let handle = self.inner.state.borrow_mut().handle.take();
        if let Some(handle) = handle {
            self.inner.layer.remove(&handle);
        }
        {
            let mut state = self.inner.state.borrow_mut();
            state.window = None;
            state.handle = None;
            state.node = None;
            state.group = None;
            state.panel = None;
            state.scrim = None;
            state.anchor = None;
            state.origin = None;
            state.frame_at_open = None;
            state.scrim_geometry = None;
            state.closing = false;
        }
        let _ = app.write(&self.inner.open, false);
        let _ = app.write(&self.inner.revealed, false);
        app.write_transient(&self.inner.fade, 0.0);
    }

    fn is_alive(&self, app: &App, window: WindowId) -> bool {
        let Some(handle) = self.inner.state.borrow().handle else {
            return false;
        };
        app.window(window)
            .is_some_and(|target| target.tree().get(handle.node()).is_ok())
    }

    fn build_group(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let fade = self.inner.fade;
        let revealed = self.inner.revealed;
        NodeBuilder::boxed()
            .id(format_smolstr!("{}:group", self.inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .build(),
            )
            .bind_style(move |cx| StylePatch {
                opacity: Some(*cx.read_transient(&fade)),
                visible: Some(*cx.read(&revealed)),
                ..StylePatch::default()
            })
            .build(tree, parent)
    }

    fn build_scrim(
        &self,
        tree: &mut NodeTree,
        parent: NodeId,
        viewport: LogicalSize,
        origin: LogicalPoint,
        scrim: Scrim,
    ) -> Result<NodeId, EngineError> {
        let theme = self.inner.theme.clone();
        let density = if scrim.density.is_finite() {
            scrim.density.clamp(0.0, 1.0)
        } else {
            1.0
        };
        NodeBuilder::boxed()
            .id(format_smolstr!("{}:scrim", self.inner.key))
            .style(scrim_style(origin, viewport))
            .bind_style(move |cx| StylePatch {
                background: Some(theme.role(cx, Role::Scrim)),
                opacity: Some(density),
                interactive: Some(false),
                ..StylePatch::default()
            })
            .a11y(A11yOverride {
                role: Some(A11yRole::Group),
                hidden: true,
                ..A11yOverride::default()
            })
            .build(tree, parent)
    }

    fn build_panel(&self, tree: &mut NodeTree, parent: NodeId) -> Result<NodeId, EngineError> {
        let theme = self.inner.theme.clone();
        let surface = self.inner.options.surface.clone();
        let mut paint = PaintBuilder::new().rounded(surface.corner_radius).build();
        paint.shadows = surface.shadows.clone();
        let background = surface.background;
        NodeBuilder::boxed()
            .id(format_smolstr!("{}:panel", self.inner.key))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .min_width_0()
                    .min_height_0()
                    .padding(surface.padding)
                    .build(),
            )
            .paint(paint)
            .bind_style(move |cx| StylePatch {
                background: Some(theme.role(cx, background)),
                ..StylePatch::default()
            })
            .build(tree, parent)
    }

    fn sync_scrim(
        &self,
        app: &mut App,
        window: WindowId,
        origin: LogicalPoint,
        viewport: LogicalSize,
    ) -> bool {
        let scrim = self.inner.state.borrow().scrim;
        let Some(scrim) = scrim else {
            return false;
        };
        {
            let mut state = self.inner.state.borrow_mut();
            if state.scrim_geometry == Some((origin, viewport)) {
                return false;
            }
            state.scrim_geometry = Some((origin, viewport));
        }
        let Some(tree) = app.window_mut(window).map(|target| target.tree_mut()) else {
            return false;
        };
        tree.set_style(scrim, scrim_style(origin, viewport)).is_ok()
    }

    fn start_fade(&self, app: &mut App, window: WindowId, group: NodeId, to: f32) {
        let fade = self.inner.fade;
        if !self.inner.options.fade {
            let revealed = *app.read(&self.inner.revealed);
            app.write_transient(&fade, if revealed { to } else { 0.0 });
            return;
        }
        let nodes = [group];
        let spec = AnimationSpec {
            duration: ComponentTokens::standard().animation_tick,
            easing: Easing::EaseOutCubic,
        };
        self.inner.router.untrack(self.inner.fade_animation.take());
        match app.animate(
            window,
            AnimatedValue::Scalar { target: fade, to },
            &nodes,
            spec,
        ) {
            Ok(id) => {
                self.inner.fade_animation.set(Some(id));
                self.inner.router.track(id, &self.inner);
            }
            Err(_) => app.write_transient(&fade, to),
        }
    }

    fn cancel_fade(&self, app: &mut App) {
        let animation = self.inner.fade_animation.take();
        self.inner.router.untrack(animation);
        if let Some(id) = animation {
            let _ = app.cancel_animation(&id);
        }
    }

    fn animation_completed(&self, app: &mut App, completion: AnimationCompletion) {
        if self.inner.fade_animation.get() != Some(completion.id) {
            return;
        }
        self.inner.fade_animation.set(None);
        if completion.kind != AnimationEndKind::Finished {
            return;
        }
        if self.inner.state.borrow().closing {
            self.close_now(app);
        }
    }
}

fn scrim_style(origin: LogicalPoint, viewport: LogicalSize) -> Style {
    StyleBuilder::new()
        .absolute()
        .left(-origin.x)
        .top(-origin.y)
        .width(viewport.width)
        .height(viewport.height)
        .build()
}

#[cfg(test)]
mod tests {

    use ui_core::math::{LogicalPoint, LogicalRect, LogicalSize, px};

    use super::{Alignment, Placement, PlacementConfig, Side, resolve_origin};

    const VIEWPORT: LogicalSize = LogicalSize::new(px(800.0), px(600.0));
    const PANEL: LogicalSize = LogicalSize::new(px(120.0), px(80.0));
    const ANCHOR: LogicalRect = LogicalRect::from_origin_size(
        LogicalPoint::new(px(300.0), px(200.0)),
        LogicalSize::new(px(100.0), px(40.0)),
    );

    fn config(flip: bool) -> PlacementConfig {
        PlacementConfig {
            gap: px(4.0),
            margin: px(4.0),
            flip,
        }
    }

    #[test]
    fn the_twelve_placements_line_up_with_the_anchor() {
        let cfg = config(false);
        let cases = [
            (
                Placement::new(Side::Bottom, Alignment::Start),
                (300.0, 244.0),
            ),
            (
                Placement::new(Side::Bottom, Alignment::Center),
                (290.0, 244.0),
            ),
            (Placement::new(Side::Bottom, Alignment::End), (280.0, 244.0)),
            (Placement::new(Side::Top, Alignment::Start), (300.0, 116.0)),
            (Placement::new(Side::Top, Alignment::Center), (290.0, 116.0)),
            (Placement::new(Side::Top, Alignment::End), (280.0, 116.0)),
            (
                Placement::new(Side::Right, Alignment::Start),
                (404.0, 200.0),
            ),
            (
                Placement::new(Side::Right, Alignment::Center),
                (404.0, 180.0),
            ),
            (Placement::new(Side::Right, Alignment::End), (404.0, 160.0)),
            (Placement::new(Side::Left, Alignment::Start), (176.0, 200.0)),
            (
                Placement::new(Side::Left, Alignment::Center),
                (176.0, 180.0),
            ),
            (Placement::new(Side::Left, Alignment::End), (176.0, 160.0)),
        ];
        for (placement, (x, y)) in cases {
            assert_eq!(
                resolve_origin(ANCHOR, PANEL, VIEWPORT, placement, cfg),
                LogicalPoint::new(px(x), px(y)),
                "{placement:?} 的落点漂移"
            );
        }
    }

    #[test]
    fn flip_moves_to_the_opposite_side_when_the_preferred_side_does_not_fit() {
        let cfg = config(true);
        let bottom = LogicalRect::new(px(300.0), px(500.0), px(100.0), px(40.0));
        assert_eq!(
            resolve_origin(bottom, PANEL, VIEWPORT, Placement::BOTTOM_START, cfg),
            LogicalPoint::new(px(300.0), px(416.0))
        );
        let right = LogicalRect::new(px(700.0), px(200.0), px(100.0), px(40.0));
        assert_eq!(
            resolve_origin(right, PANEL, VIEWPORT, Placement::RIGHT_START, cfg),
            LogicalPoint::new(px(576.0), px(200.0))
        );
        let tight = LogicalRect::new(px(300.0), px(120.0), px(100.0), px(40.0));
        assert_eq!(
            resolve_origin(
                tight,
                PANEL,
                LogicalSize::new(px(800.0), px(200.0)),
                Placement::BOTTOM_START,
                cfg
            ),
            LogicalPoint::new(px(300.0), px(36.0))
        );
    }

    #[test]
    fn flip_can_be_disabled() {
        let cfg = config(false);
        let bottom = LogicalRect::new(px(300.0), px(500.0), px(100.0), px(40.0));
        assert_eq!(
            resolve_origin(bottom, PANEL, VIEWPORT, Placement::BOTTOM_START, cfg),
            LogicalPoint::new(px(300.0), px(516.0)),
            "关掉 flip 后 Bottom 被 clamp 到视口底边"
        );
    }

    #[test]
    fn clamping_keeps_the_panel_inside_the_viewport() {
        let cfg = config(false);
        let corner = LogicalRect::new(px(790.0), px(590.0), px(10.0), px(10.0));
        assert_eq!(
            resolve_origin(corner, PANEL, VIEWPORT, Placement::BOTTOM_END, cfg),
            LogicalPoint::new(
                px(800.0 - 4.0) - PANEL.width,
                px(600.0 - 4.0) - PANEL.height
            )
        );
        let top_left = LogicalRect::new(px(0.0), px(0.0), px(10.0), px(10.0));
        assert_eq!(
            resolve_origin(top_left, PANEL, VIEWPORT, Placement::TOP_START, cfg),
            LogicalPoint::new(px(4.0), px(4.0))
        );
        assert_eq!(
            resolve_origin(
                ANCHOR,
                LogicalSize::new(px(900.0), px(700.0)),
                VIEWPORT,
                Placement::BOTTOM_START,
                cfg
            ),
            LogicalPoint::new(px(4.0), px(4.0))
        );
    }

    #[test]
    fn non_finite_inputs_and_negative_parameters_are_safe() {
        let cfg = PlacementConfig::default();
        assert_eq!(
            resolve_origin(
                LogicalRect::new(px(f32::NAN), px(0.0), px(10.0), px(10.0)),
                PANEL,
                VIEWPORT,
                Placement::BOTTOM_START,
                cfg
            ),
            LogicalPoint::ZERO
        );
        assert_eq!(
            resolve_origin(
                ANCHOR,
                LogicalSize::new(px(f32::INFINITY), px(10.0)),
                VIEWPORT,
                Placement::BOTTOM_START,
                cfg
            ),
            LogicalPoint::ZERO
        );
        let negative = PlacementConfig {
            gap: px(-5.0),
            margin: px(-5.0),
            flip: true,
        };
        assert_eq!(
            resolve_origin(ANCHOR, PANEL, VIEWPORT, Placement::BOTTOM_START, negative),
            LogicalPoint::new(px(300.0), px(240.0)),
            "负间隙 / 负边距按 0 处理"
        );
    }

    #[test]
    fn non_finite_config_falls_back_to_the_origin() {
        for broken in [
            PlacementConfig {
                gap: px(f32::INFINITY),
                margin: px(4.0),
                flip: true,
            },
            PlacementConfig {
                gap: px(4.0),
                margin: px(f32::NAN),
                flip: true,
            },
        ] {
            assert_eq!(
                resolve_origin(ANCHOR, PANEL, VIEWPORT, Placement::BOTTOM_START, broken),
                LogicalPoint::ZERO,
                "非有限定位参数必须回退原点：{broken:?}"
            );
        }
    }

    #[test]
    fn cross_axis_overflow_does_not_flip_for_all_twelve_placements() {
        let cfg = config(true);
        let cases = [
            (
                LogicalRect::new(px(760.0), px(200.0), px(40.0), px(20.0)),
                Placement::BOTTOM_START,
                (676.0, 224.0),
            ),
            (
                LogicalRect::new(px(760.0), px(200.0), px(40.0), px(20.0)),
                Placement::BOTTOM,
                (676.0, 224.0),
            ),
            (
                LogicalRect::new(px(10.0), px(200.0), px(40.0), px(20.0)),
                Placement::BOTTOM_END,
                (4.0, 224.0),
            ),
            (
                LogicalRect::new(px(760.0), px(300.0), px(40.0), px(20.0)),
                Placement::TOP_START,
                (676.0, 216.0),
            ),
            (
                LogicalRect::new(px(760.0), px(300.0), px(40.0), px(20.0)),
                Placement::TOP,
                (676.0, 216.0),
            ),
            (
                LogicalRect::new(px(10.0), px(300.0), px(40.0), px(20.0)),
                Placement::TOP_END,
                (4.0, 216.0),
            ),
            (
                LogicalRect::new(px(200.0), px(590.0), px(40.0), px(20.0)),
                Placement::RIGHT_START,
                (244.0, 516.0),
            ),
            (
                LogicalRect::new(px(200.0), px(590.0), px(40.0), px(20.0)),
                Placement::RIGHT,
                (244.0, 516.0),
            ),
            (
                LogicalRect::new(px(200.0), px(590.0), px(40.0), px(20.0)),
                Placement::RIGHT_END,
                (244.0, 516.0),
            ),
            (
                LogicalRect::new(px(300.0), px(590.0), px(40.0), px(20.0)),
                Placement::LEFT_START,
                (176.0, 516.0),
            ),
            (
                LogicalRect::new(px(300.0), px(590.0), px(40.0), px(20.0)),
                Placement::LEFT,
                (176.0, 516.0),
            ),
            (
                LogicalRect::new(px(300.0), px(590.0), px(40.0), px(20.0)),
                Placement::LEFT_END,
                (176.0, 516.0),
            ),
        ];
        for (anchor, placement, (x, y)) in cases {
            assert_eq!(
                resolve_origin(anchor, PANEL, VIEWPORT, placement, cfg),
                LogicalPoint::new(px(x), px(y)),
                "{placement:?} 横轴越界只 clamp、不翻转（anchor {anchor:?}）"
            );
        }
    }

    #[test]
    fn main_axis_overflow_flips_for_all_twelve_placements() {
        let cfg = config(true);
        let narrow = LogicalRect::new(px(340.0), px(590.0), px(100.0), px(20.0));
        let low = LogicalRect::new(px(340.0), px(0.0), px(100.0), px(20.0));
        let right = LogicalRect::new(px(760.0), px(260.0), px(40.0), px(40.0));
        let left = LogicalRect::new(px(0.0), px(260.0), px(40.0), px(40.0));
        let cases = [
            (narrow, Placement::BOTTOM_START, (340.0, 506.0)),
            (narrow, Placement::BOTTOM, (330.0, 506.0)),
            (narrow, Placement::BOTTOM_END, (320.0, 506.0)),
            (low, Placement::TOP_START, (340.0, 24.0)),
            (low, Placement::TOP, (330.0, 24.0)),
            (low, Placement::TOP_END, (320.0, 24.0)),
            (right, Placement::RIGHT_START, (636.0, 260.0)),
            (right, Placement::RIGHT, (636.0, 240.0)),
            (right, Placement::RIGHT_END, (636.0, 220.0)),
            (left, Placement::LEFT_START, (44.0, 260.0)),
            (left, Placement::LEFT, (44.0, 240.0)),
            (left, Placement::LEFT_END, (44.0, 220.0)),
        ];
        for (anchor, placement, (x, y)) in cases {
            assert_eq!(
                resolve_origin(anchor, PANEL, VIEWPORT, placement, cfg),
                LogicalPoint::new(px(x), px(y)),
                "{placement:?} 主轴放不下必须翻到对边（anchor {anchor:?}）"
            );
        }
    }
}

#[cfg(test)]
mod ui_tests {

    use std::time::Duration;

    use smol_str::format_smolstr;
    use ui_core::{
        a11y::A11yRole,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId},
        paint::{
            color::Color,
            style_builder::{PaintBuilder, StyleBuilder},
        },
        test_support::app::TestApp,
        window::{FrameOutcome, WindowId},
    };

    use super::{
        Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayA11y, OverlayError, OverlayOptions,
        OverlaySurface, Scrim,
    };
    use crate::{
        kit::layer::Layer,
        theme::{appearance::Appearance, components::ComponentTokens, handle::ThemeHandle},
    };

    const VIEWPORT: LogicalSize = LogicalSize::new(px(800.0), px(600.0));
    const ANCHOR: LogicalRect = LogicalRect::from_origin_size(
        LogicalPoint::new(px(300.0), px(200.0)),
        LogicalSize::new(px(100.0), px(40.0)),
    );
    const CONTENT: LogicalSize = LogicalSize::new(px(120.0), px(80.0));

    fn build_content(
        tree: &mut ui_core::node::tree::NodeTree,
        panel: NodeId,
    ) -> Result<(), ui_core::error::EngineError> {
        NodeBuilder::boxed()
            .id("content")
            .style(
                StyleBuilder::new()
                    .width(CONTENT.width)
                    .height(CONTENT.height)
                    .build(),
            )
            .paint(
                PaintBuilder::new()
                    .background(ui_core::paint::color::Color::WHITE)
                    .build(),
            )
            .build(tree, panel)?;
        Ok(())
    }

    struct Harness {
        app: TestApp,
        window: WindowId,
        root: NodeId,
        theme: ThemeHandle,
        layer: Layer,
        router: AnimationRouter,
    }

    impl Harness {
        fn new() -> Self {
            let mut app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let theme = ThemeHandle::new(app.app_mut(), Appearance::Light);
            Self {
                app,
                window,
                root,
                theme,
                layer: Layer::new(),
                router: AnimationRouter::new(),
            }
        }

        fn overlay(&mut self, key: &str, options: OverlayOptions) -> Overlay {
            Overlay::new(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                key,
                options,
            )
        }

        fn round(&mut self, by: Duration, overlay: &Overlay) {
            self.app.advance(by);
            let _ = overlay.sync(self.app.app_mut());
            let _ = self.app.drain().expect("帧");
        }

        fn anchor(&mut self) -> NodeId {
            self.node("anchor", ANCHOR, None)
        }

        fn node(&mut self, id: &str, rect: LogicalRect, action: Option<Action>) -> NodeId {
            let mut builder = NodeBuilder::boxed().id(id.to_owned()).layout_override(rect);
            if let Some(action) = action {
                builder = builder.on_click(action);
            }
            builder
                .build(
                    self.app
                        .app_mut()
                        .window_mut(self.window)
                        .expect("窗口")
                        .tree_mut(),
                    self.root,
                )
                .expect("节点")
        }

        fn open(&mut self, overlay: &Overlay, anchor: Anchor) -> bool {
            let opened = overlay
                .open(self.app.app_mut(), self.window, anchor, build_content)
                .expect("打开浮层");
            self.settle(overlay);
            opened
        }

        fn settle(&mut self, overlay: &Overlay) {
            for _ in 0..3 {
                let _ = self.app.drain().expect("帧");
                let _ = overlay.sync(self.app.app_mut());
            }
            let _ = self.app.drain().expect("帧");
        }

        fn bounds(&self, node: NodeId) -> LogicalRect {
            self.app.probe().bounds(node).expect("布局盒")
        }

        fn click(&mut self, at: LogicalPoint) {
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
        }
    }

    fn options() -> OverlayOptions {
        OverlayOptions {
            fade: false,
            ..OverlayOptions::default()
        }
    }

    #[test]
    fn the_panel_opens_below_its_anchor_after_layout_settles() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));

        let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert_eq!(
            panel.origin,
            LogicalPoint::new(ANCHOR.origin.x, ANCHOR.max().y + 4.0)
        );
        assert_eq!(
            panel.size,
            LogicalSize::new(CONTENT.width + 8.0, CONTENT.height + 8.0),
            "面板尺寸 = 内容 + 两侧 4 px 内边距"
        );
        assert!(overlay.is_open(harness.app.app()));
        assert_eq!(
            overlay.progress(harness.app.app()),
            1.0,
            "关掉淡出后立即不透明"
        );
    }

    #[test]
    fn a_panel_near_the_bottom_flips_above_the_anchor() {
        let mut harness = Harness::new();
        let anchor = harness.node(
            "anchor",
            LogicalRect::new(px(300.0), px(560.0), px(100.0), px(40.0)),
            None,
        );
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert!(
            (panel.max().y - (560.0 - 4.0)).abs().value() < 0.5,
            "翻到上方后底边应贴锚点顶部 − gap：{panel:?}"
        );
    }

    #[test]
    fn a_press_outside_closes_the_overlay_and_updates_the_state() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let node = overlay.overlay_node().expect("浮层节点");

        harness.click(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = overlay.sync(harness.app.app_mut());
        assert!(!overlay.is_open(harness.app.app()), "外部点击必须关闭浮层");
        assert!(overlay.overlay_node().is_none());
        assert!(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .get(node)
                .is_err(),
            "浮层节点已卸载"
        );
    }

    #[test]
    fn escape_closes_the_overlay() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
        let _ = harness.app.drain().expect("帧");
        let _ = overlay.sync(harness.app.app_mut());
        assert!(!overlay.is_open(harness.app.app()), "Esc 关闭最上层浮层");
        assert!(overlay.overlay_node().is_none());
    }

    #[test]
    fn a_modal_overlay_traps_focus_and_restores_it_on_close() {
        let mut harness = Harness::new();
        let trigger = Action::Custom(format_smolstr!("trigger.click"));
        let trigger_node = harness.node("trigger", ANCHOR, Some(trigger));
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(trigger_node));
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger_node),
            "前置：焦点在触发器上"
        );

        let overlay = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                dismiss: DismissPolicy::Explicit,
                restore_focus: true,
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::dialog("示例对话框"),
                fade: false,
                ..OverlayOptions::default()
            },
        );
        let inner = Action::Custom(format_smolstr!("inner.click"));
        let opened = overlay
            .open(
                harness.app.app_mut(),
                harness.window,
                Anchor::node(trigger_node),
                move |tree, panel| {
                    NodeBuilder::boxed()
                        .id("inner")
                        .style(StyleBuilder::new().width(px(80.0)).height(px(24.0)).build())
                        .on_click(inner)
                        .build(tree, panel)?;
                    Ok(())
                },
            )
            .expect("打开对话框");
        assert!(opened);
        harness.settle(&overlay);

        let _ = harness.app.app_mut().focus_next(harness.window);
        let focused = harness.app.app().focused(harness.window);
        assert_eq!(
            focused.map(|node| harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .debug_id(node)
                .map(str::to_owned)),
            Some(Some("inner".to_owned())),
            "焦点作用域把 Tab 限制在模态内"
        );

        overlay.close_now(harness.app.app_mut());
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger_node),
            "关闭后焦点必须还给 opener"
        );
    }

    #[test]
    fn a_modal_overlay_draws_a_scrim_and_a_plain_one_does_not() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let plain = harness.overlay("plain", options());
        assert!(harness.open(&plain, Anchor::node(anchor)));
        assert!(plain.scrim_node().is_none(), "非模态浮层没有遮罩");

        let modal = harness.overlay(
            "modal",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::new(0.5)),
                dismiss: DismissPolicy::Explicit,
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(
            modal
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    |_, _| Ok(())
                )
                .expect("打开模态")
        );
        harness.settle(&modal);
        let scrim = modal.scrim_node().expect("模态浮层有遮罩");
        let bounds = harness.bounds(scrim);
        assert_eq!(bounds.origin, LogicalPoint::ZERO, "遮罩铺满窗口");
        assert_eq!(bounds.size, VIEWPORT, "遮罩覆盖整个视口");
    }

    #[test]
    fn clicking_the_scrim_dismisses_an_outside_click_overlay() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        assert!(overlay.scrim_node().is_some());
        harness.click(LogicalPoint::new(px(20.0), px(20.0)));
        let _ = overlay.sync(harness.app.app_mut());
        assert!(!overlay.is_open(harness.app.app()), "点遮罩 = 点外部");
    }

    #[test]
    fn closing_fades_out_and_a_finished_animation_unloads_in_the_same_frame() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                fade: true,
                ..options()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("帧");
        assert_eq!(overlay.progress(harness.app.app()), 1.0, "淡入完成");
        let node = overlay.overlay_node().expect("浮层节点");

        assert!(overlay.close(harness.app.app_mut()));
        assert!(
            overlay.is_open(harness.app.app()),
            "关闭中仍然打开（淡出未结束）"
        );
        let alive = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .get(node)
            .is_ok();
        assert!(alive, "淡出期间节点保留");

        let mut unloaded_in_the_completion_frame = false;
        for _ in 0..20 {
            harness.app.advance(Duration::from_millis(16));
            let _ = overlay.sync(harness.app.app_mut());
            let _ = harness.app.drain().expect("帧");
            if !harness.app.app().animations_running(harness.window) {
                assert!(
                    overlay.overlay_node().is_none(),
                    "动画完成的那一轮 drain 之后节点必须已卸载（完成回调同帧收尾；\
                     等下一轮 sync 的旧形态会在这里变红）"
                );
                unloaded_in_the_completion_frame = true;
                break;
            }
        }
        assert!(
            unloaded_in_the_completion_frame,
            "淡出必须在 20 轮内走完并触发完成回调"
        );
        assert!(!overlay.is_open(harness.app.app()), "淡出结束后关闭");
        let alive = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree()
            .get(node)
            .is_ok();
        assert!(!alive, "淡出结束后节点卸载");
        assert_eq!(overlay.progress(harness.app.app()), 0.0);
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "收尾后不得残留出帧");
    }

    #[test]
    fn two_overlays_share_one_animation_completion_route() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let first = harness.overlay(
            "first",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: true,
                ..options()
            },
        );
        let second = harness.overlay(
            "second",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: true,
                ..options()
            },
        );
        assert!(harness.open(&first, Anchor::node(anchor)));
        assert!(harness.open(&second, Anchor::node(anchor)));
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        assert_eq!(first.progress(harness.app.app()), 1.0, "淡入完成");
        assert_eq!(second.progress(harness.app.app()), 1.0, "淡入完成");

        assert!(first.close(harness.app.app_mut()));
        assert!(second.close(harness.app.app_mut()));
        for _ in 0..16 {
            harness.round(Duration::from_millis(16), &first);
            if first.overlay_node().is_none() && second.overlay_node().is_none() {
                break;
            }
        }
        assert!(!first.is_open(harness.app.app()), "第一个浮层已收尾");
        assert!(!second.is_open(harness.app.app()), "第二个浮层已收尾");
        assert!(first.overlay_node().is_none());
        assert!(second.overlay_node().is_none());
    }

    #[test]
    fn an_interleaved_replaced_completion_is_dispatched_by_animation_id() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let first = harness.overlay(
            "first",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: true,
                ..options()
            },
        );
        let second = harness.overlay(
            "second",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                fade: true,
                ..options()
            },
        );
        assert!(harness.open(&first, Anchor::node(anchor)));
        assert!(harness.open(&second, Anchor::node(anchor)));
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        assert_eq!(first.progress(harness.app.app()), 1.0, "前置：A 淡入完成");
        assert_eq!(second.progress(harness.app.app()), 1.0, "前置：B 淡入完成");

        assert!(first.close(harness.app.app_mut()));
        harness.app.advance(Duration::from_millis(32));
        let _ = first.sync(harness.app.app_mut());
        let _ = second.sync(harness.app.app_mut());
        let _ = harness.app.drain().expect("淡出帧");
        assert!(
            first.overlay_node().is_some(),
            "前置：A 还在淡出窗口内（节点未卸载）"
        );

        assert!(
            !first
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content,
                )
                .expect("A 重开"),
            "同窗重入返回 false（只换锚点）"
        );
        assert!(first.is_open(harness.app.app()));
        assert!(second.close(harness.app.app_mut()));

        let mut settled = false;
        for _ in 0..20 {
            harness.app.advance(Duration::from_millis(16));
            let _ = first.sync(harness.app.app_mut());
            let _ = second.sync(harness.app.app_mut());
            let _ = harness.app.drain().expect("帧");
            if !harness.app.app().animations_running(harness.window) {
                settled = true;
                break;
            }
        }
        assert!(settled, "两条在飞动画必须在 20 轮内走完");
        assert!(
            first.overlay_node().is_some(),
            "重开的 A 不得被陈旧的 `Replaced` 事件摘掉（FIFO 分发会摘错消费者）"
        );
        assert!(first.is_open(harness.app.app()));
        assert_eq!(
            first.progress(harness.app.app()),
            1.0,
            "A 淡回不透明并留在屏上"
        );
        assert!(
            second.overlay_node().is_none(),
            "B 的完成事件必须命中 B 自己（FIFO 会把 A 的登记摘掉、让 B 永远收不了尾）"
        );
        assert!(!second.is_open(harness.app.app()), "B 已收尾");
    }

    #[test]
    fn a_settled_overlay_requests_no_more_frames() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        assert!(!overlay.sync(harness.app.app_mut()), "稳态 sync 必须零工作");
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "稳态不得持续出帧");

        overlay.close_now(harness.app.app_mut());
        let _ = harness.app.drain().expect("卸载帧");
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "关闭后不得有残留帧");
    }

    #[test]
    fn the_overlay_follows_its_anchor_inside_a_scrolling_container() {
        let mut harness = Harness::new();
        let viewport_node = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("scroller")
                .style(
                    StyleBuilder::new()
                        .absolute()
                        .left(px(0.0))
                        .top(px(0.0))
                        .width(px(300.0))
                        .height(px(200.0))
                        .overflow_hidden()
                        .build(),
                )
                .build(tree, harness.root)
                .expect("滚动容器")
        };
        let scroll_handle = harness
            .app
            .app_mut()
            .make_scroll(
                harness.window,
                viewport_node,
                ui_core::interaction::ScrollConfig::default(),
            )
            .expect("滚动容器登记");
        harness
            .app
            .app_mut()
            .set_scroll_content(&scroll_handle, LogicalSize::new(px(300.0), px(1000.0)))
            .expect("内容尺寸");
        let anchor = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("anchor")
                .layout_override(LogicalRect::new(px(40.0), px(400.0), px(100.0), px(40.0)))
                .build(tree, viewport_node)
                .expect("锚点")
        };
        let overlay = harness.overlay("menu", options());
        assert!(
            overlay
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    |_, _| Ok(())
                )
                .expect("打开")
        );
        harness.settle(&overlay);
        let before = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert!(
            (before.origin.y - 444.0).abs().value() < 0.5,
            "未滚动：{before:?}"
        );

        harness
            .app
            .app_mut()
            .scroll_to(&scroll_handle, LogicalPoint::new(px(0.0), px(300.0)))
            .expect("滚动");
        harness.settle(&overlay);
        let after = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert!(
            (after.origin.y - (before.origin.y - 300.0)).abs().value() < 0.5,
            "滚动 300 后浮层跟随锚点上移：before={before:?} after={after:?}"
        );
    }

    #[test]
    fn an_anchor_that_disappears_closes_the_overlay() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            tree.remove(anchor).expect("移除锚点");
        }
        let _ = harness.app.drain().expect("帧");
        assert!(overlay.sync(harness.app.app_mut()), "sync 必须发现锚点消失");
        assert!(!overlay.is_open(harness.app.app()), "锚点消失 ⇒ 浮层关闭");
    }

    #[test]
    fn the_overlay_wrapper_declares_its_a11y_role() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::dialog("确认"),
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(
            overlay
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("打开")
        );
        let _ = harness.app.drain().expect("帧");
        let wrapper = overlay.overlay_node().expect("浮层包装节点");
        let panel = overlay.panel_node().expect("面板");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(wrapper).expect("包装节点 a11y");
        assert_eq!(record.role, A11yRole::Dialog, "包装节点必须显式声明 Dialog");
        assert_eq!(record.label_text(), Some("确认"));
        assert!(record.modal, "模态对话框必须声明 modal");
        let panel_record = snapshot.node_for(panel).expect("面板 a11y");
        assert_eq!(
            panel_record.role,
            A11yRole::GenericContainer,
            "面板不得重复声明同一角色（否则 AT 读两层对话框）"
        );
    }

    #[test]
    fn a_surface_change_only_affects_dye_not_geometry() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        for (key, surface) in [
            ("menu", OverlaySurface::menu()),
            ("dialog", OverlaySurface::dialog()),
            ("tooltip", OverlaySurface::tooltip()),
        ] {
            let padding = surface.padding;
            let overlay = harness.overlay(
                key,
                OverlayOptions {
                    surface,
                    fade: false,
                    dismiss: DismissPolicy::Explicit,
                    ..OverlayOptions::default()
                },
            );
            assert!(harness.open(&overlay, Anchor::node(anchor)));
            let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
            assert_eq!(
                panel.size,
                LogicalSize::new(
                    CONTENT.width + padding * 2.0,
                    CONTENT.height + padding * 2.0
                ),
                "{key} 面板尺寸只由内边距决定"
            );
            overlay.close_now(harness.app.app_mut());
            let _ = harness.app.drain().expect("帧");
        }
    }

    #[test]
    fn surfaces_cast_the_legacy_multi_layer_shadows() {
        let tokens = ComponentTokens::standard();
        let menu = OverlaySurface::menu();
        let layers: Vec<_> = menu
            .shadows
            .iter()
            .map(|shadow| (shadow.offset, shadow.blur, shadow.spread, shadow.color))
            .collect();
        assert_eq!(
            layers,
            vec![
                (
                    LogicalPoint::new(px(0.0), px(1.0)),
                    px(2.0),
                    px(0.0),
                    Color::BLACK.with_opacity(tokens.shadow.menu_alpha),
                ),
                (
                    LogicalPoint::new(px(0.0), px(3.0)),
                    px(6.0),
                    px(-2.0),
                    Color::BLACK.with_opacity(tokens.shadow.menu_alpha),
                ),
            ],
            "菜单必须是旧实现的双层接触阴影（含负 spread）"
        );

        let dialog = OverlaySurface::dialog();
        let layers: Vec<_> = dialog
            .shadows
            .iter()
            .map(|shadow| (shadow.offset, shadow.blur, shadow.spread))
            .collect();
        assert_eq!(
            layers,
            vec![(LogicalPoint::new(px(0.0), px(25.0)), px(50.0), px(-12.0))],
            "对话框必须是旧实现的大偏移单层阴影"
        );
        assert_eq!(
            dialog.shadows.iter().next().map(|shadow| shadow.color),
            Some(Color::BLACK.with_opacity(tokens.shadow.dialog_alpha)),
            "对话框阴影色 = 主题 dialog alpha"
        );

        assert!(
            OverlaySurface::tooltip().shadows.is_empty(),
            "tooltip 旧实现没有阴影"
        );
    }

    #[test]
    fn the_fade_group_root_holds_the_scrim_panel_and_content() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::dialog("确认"),
                fade: true,
                ..OverlayOptions::default()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let wrapper = overlay.overlay_node().expect("包装节点");
        let panel = overlay.panel_node().expect("面板");
        let scrim = overlay.scrim_node().expect("遮罩");
        let tree = harness
            .app
            .app()
            .window(harness.window)
            .expect("窗口")
            .tree();
        let children = tree.get(wrapper).expect("包装节点").children();
        assert_eq!(
            children.len(),
            1,
            "包装节点下只能有一层 kit 组根，实际 {children:?}"
        );
        let group = children[0];
        assert_eq!(
            tree.get(group).expect("组根").children(),
            [scrim, panel],
            "遮罩（先画）与面板（后画）都必须是组根的子节点"
        );
        assert_eq!(tree.get(panel).expect("面板").parent(), Some(group));
        assert_eq!(tree.get(group).expect("组根").parent(), Some(wrapper));
        assert!(
            tree.debug_id(group)
                .is_some_and(|id| id.ends_with(":group")),
            "组根 id 带 `:group` 后缀（调试 / 断言用）"
        );
    }

    #[test]
    fn the_fade_dims_the_whole_group_in_the_readback() {
        let _guard = ui_core::test_support::snapshot::gpu_guard();
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "fade",
            OverlayOptions {
                surface: OverlaySurface::tooltip(),
                a11y: OverlayA11y::tooltip("提示"),
                fade: true,
                dismiss: DismissPolicy::Explicit,
                ..OverlayOptions::default()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        assert_eq!(overlay.progress(harness.app.app()), 1.0, "前置：淡入完成");

        assert!(overlay.close(harness.app.app_mut()));
        let mut progress = 1.0;
        for _ in 0..20 {
            harness.app.advance(Duration::from_millis(8));
            let _ = overlay.sync(harness.app.app_mut());
            let _ = harness.app.drain().expect("淡出帧");
            progress = overlay.progress(harness.app.app());
            if (0.4..=0.6).contains(&progress) {
                break;
            }
        }
        assert!(
            (0.4..=0.6).contains(&progress),
            "前置：必须取到淡出中段，实际 progress = {progress}"
        );
        let expected_shell = (progress * 255.0).round() as i32;
        let expected_content = (progress * (2.0 - progress) * 255.0).round() as i32;

        let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        let padding = overlay.options().surface.padding;
        let padding_x = (panel.origin.x + panel.size.width * 0.5).value() as u32;
        let padding_y = (panel.origin.y + padding * 0.5).value() as u32;
        let content_x = (panel.origin.x + padding + CONTENT.width * 0.5).value() as u32;
        let content_y = (panel.origin.y + padding + CONTENT.height * 0.5).value() as u32;
        let image = harness
            .app
            .read_pixels()
            .unwrap_or_else(|error| panic!("离屏读回失败：{error:?}"));
        let shell_alpha =
            i32::from(ui_core::test_support::pixels::pixel(&image, padding_x, padding_y)[3]);
        let content_alpha =
            i32::from(ui_core::test_support::pixels::pixel(&image, content_x, content_y)[3]);
        assert!(
            (shell_alpha - expected_shell).abs() <= 8,
            "面板背景 alpha 应为组不透明度 {progress:.3}×255≈{expected_shell}，实测 {shell_alpha}"
        );
        assert!(
            (content_alpha - expected_content).abs() <= 8,
            "内容像素必须随组根一起淡：应是 p+p(1−p)≈{expected_content}，实测 {content_alpha}\
             （只淡壳的实现会停在 255）"
        );
        assert!(
            content_alpha < 240,
            "内容像素没有变暗 ⇒ 组不透明度没有级联到子节点"
        );
    }

    #[test]
    fn an_engine_side_close_cleans_the_layer_entry_and_stops_enforcing() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let background = harness.overlay(
            "background",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                ..options()
            },
        );
        let foreground = harness.overlay("foreground", options());
        assert!(harness.open(&background, Anchor::node(anchor)));
        assert!(harness.open(&foreground, Anchor::node(anchor)));
        assert_eq!(harness.layer.len(harness.window), 2);

        harness.click(LogicalPoint::new(px(10.0), px(10.0)));
        assert!(
            foreground.sync(harness.app.app_mut()),
            "sync 必须发现引擎侧关闭"
        );
        assert!(!foreground.is_open(harness.app.app()));
        assert_eq!(
            harness.layer.len(harness.window),
            1,
            "关闭路径必须摘掉 Layer 登记（否则登记泄漏）"
        );
        assert!(
            !background.sync(harness.app.app_mut()),
            "稳态：没有登记失配 ⇒ enforce 零重排"
        );
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(
            outcome,
            FrameOutcome::Idle,
            "关闭收尾后不得每轮出帧（R03 的空转）"
        );
    }

    #[test]
    fn reopening_after_an_engine_side_close_builds_a_new_overlay() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                dismiss: DismissPolicy::Explicit,
                ..options()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        let old_node = overlay.overlay_node().expect("浮层节点");
        let _ = harness
            .app
            .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
        let _ = harness.app.drain().expect("帧");
        assert!(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .get(old_node)
                .is_err(),
            "Esc 已卸载引擎节点（kit 尚未 sync）"
        );
        assert!(
            overlay.is_open(harness.app.app()),
            "sync 之前 kit 状态仍是打开"
        );

        let reopened = overlay
            .open(
                harness.app.app_mut(),
                harness.window,
                Anchor::node(anchor),
                build_content,
            )
            .expect("重开");
        assert!(reopened, "引擎已关闭时重开必须真的打开，不能静默丢掉");
        let new_node = overlay.overlay_node().expect("新浮层节点");
        assert_ne!(new_node, old_node, "重开建的是新浮层");
        assert!(overlay.is_open(harness.app.app()));
        assert_eq!(
            harness.layer.len(harness.window),
            1,
            "旧登记被替换，不得重复登记"
        );
    }

    #[test]
    fn closing_twice_during_the_fade_is_idempotent() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                fade: true,
                ..options()
            },
        );
        assert!(harness.open(&overlay, Anchor::node(anchor)));
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("帧");
        let _ = overlay.sync(harness.app.app_mut());
        assert_eq!(overlay.progress(harness.app.app()), 1.0, "淡入完成");
        let node = overlay.overlay_node().expect("浮层节点");

        assert!(overlay.close(harness.app.app_mut()), "第一次关闭");
        assert!(
            overlay.close(harness.app.app_mut()),
            "第二次关闭幂等返回 true"
        );
        assert!(
            overlay.is_open(harness.app.app()),
            "仍在关闭中（is_open 要到淡出收尾才 false）"
        );
        assert!(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .get(node)
                .is_ok(),
            "第二次 close 不得跳过淡出直接卸载节点"
        );

        for _ in 0..16 {
            harness.round(Duration::from_millis(16), &overlay);
            if overlay.overlay_node().is_none() {
                break;
            }
        }
        assert!(!overlay.is_open(harness.app.app()));
        assert!(
            harness
                .app
                .app()
                .window(harness.window)
                .expect("窗口")
                .tree()
                .get(node)
                .is_err(),
            "节点在淡出结束后卸载"
        );
        assert!(
            !overlay.close(harness.app.app_mut()),
            "已关闭后再关返回 false"
        );
    }

    #[test]
    fn a_fade_less_panel_is_hidden_on_its_first_frame() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay("menu", options());
        assert!(
            overlay
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("打开")
        );
        assert_eq!(
            overlay.progress(harness.app.app()),
            0.0,
            "首帧不显示（等待实测）"
        );
        let _ = harness.app.drain().expect("首帧");
        assert_eq!(
            overlay.progress(harness.app.app()),
            0.0,
            "首次 sync 之前仍不显示"
        );

        assert!(overlay.sync(harness.app.app_mut()), "首次实测后揭开面板");
        assert_eq!(
            overlay.progress(harness.app.app()),
            1.0,
            "fade=false 揭开即不透明"
        );
        let _ = harness.app.drain().expect("第二帧");
        let panel = harness.bounds(overlay.overlay_node().expect("浮层节点"));
        assert_eq!(
            panel.origin,
            LogicalPoint::new(ANCHOR.origin.x, ANCHOR.max().y + 4.0),
            "第二帧在实测后的正确位置"
        );
    }

    #[test]
    fn the_wrapper_declares_menu_dialog_and_alert_dialog_roles_and_the_scrim_is_hidden() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let anchor = harness.anchor();
        let menu = harness.overlay(
            "menu",
            OverlayOptions {
                a11y: OverlayA11y::menu("文件"),
                ..options()
            },
        );
        assert!(
            menu.open(
                harness.app.app_mut(),
                harness.window,
                Anchor::node(anchor),
                build_content
            )
            .expect("打开菜单")
        );
        let _ = harness.app.drain().expect("帧");
        let menu_wrapper = menu.overlay_node().expect("菜单包装节点");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(menu_wrapper).expect("菜单包装 a11y");
        assert_eq!(record.role, A11yRole::Menu, "菜单包装节点必须声明 Menu");
        assert_eq!(record.label_text(), Some("文件"));

        let modal = harness.overlay(
            "dialog",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::dialog("确认"),
                dismiss: DismissPolicy::Explicit,
                ..options()
            },
        );
        assert!(
            modal
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("打开模态")
        );
        let _ = harness.app.drain().expect("帧");
        let modal_wrapper = modal.overlay_node().expect("对话框包装节点");
        let scrim = modal.scrim_node().expect("模态遮罩");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(modal_wrapper).expect("对话框包装 a11y");
        assert_eq!(record.role, A11yRole::Dialog);
        assert_eq!(record.label_text(), Some("确认"));
        let record = snapshot.node_for(scrim).expect("遮罩 a11y");
        assert_eq!(record.role, A11yRole::Group, "遮罩是装饰组");
        assert!(record.hidden, "遮罩必须对 AT 隐藏");

        let alert = harness.overlay(
            "alert",
            OverlayOptions {
                modal: true,
                scrim: Some(Scrim::default()),
                surface: OverlaySurface::dialog(),
                a11y: OverlayA11y::alert_dialog("删除？"),
                dismiss: DismissPolicy::Explicit,
                ..options()
            },
        );
        assert!(
            alert
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("打开警告对话框")
        );
        let _ = harness.app.drain().expect("帧");
        let alert_wrapper = alert.overlay_node().expect("警告对话框包装节点");
        let snapshot = harness.app.a11y().expect("a11y 快照");
        let record = snapshot.node_for(alert_wrapper).expect("警告对话框 a11y");
        assert_eq!(record.role, A11yRole::AlertDialog);
        assert_eq!(record.label_text(), Some("删除？"));
        assert!(record.modal, "警告对话框是模态");
    }

    #[test]
    fn a_missing_anchor_does_not_open_an_overlay() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            tree.remove(anchor).expect("移除锚点");
        }
        let _ = harness.app.drain().expect("帧");
        let overlay = harness.overlay("menu", options());
        assert!(
            !overlay
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    Anchor::node(anchor),
                    build_content
                )
                .expect("锚点失效不报错，只是不开"),
            "锚点失效 ⇒ open 返回 false"
        );
        assert!(overlay.overlay_node().is_none(), "不得建浮层节点");
        assert!(!overlay.is_open(harness.app.app()));
        assert_eq!(harness.layer.len(harness.window), 0);
    }

    #[test]
    fn an_invalid_placement_config_is_a_typed_error() {
        let mut harness = Harness::new();
        let anchor = harness.anchor();
        let overlay = harness.overlay(
            "menu",
            OverlayOptions {
                gap: px(f32::INFINITY),
                ..options()
            },
        );
        let error = overlay
            .open(
                harness.app.app_mut(),
                harness.window,
                Anchor::node(anchor),
                build_content,
            )
            .expect_err("非有限 gap 必须报错");
        assert!(
            matches!(error, OverlayError::PlacementConfig { gap, .. } if !gap.is_finite()),
            "必须是强类型 PlacementConfig 错误，实际 {error:?}"
        );
        assert!(overlay.overlay_node().is_none(), "配置错误不得建节点");
        assert_eq!(harness.layer.len(harness.window), 0);
    }
}
