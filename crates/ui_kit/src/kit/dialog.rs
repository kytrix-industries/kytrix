use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::Position;
use ui_core::{
    a11y::A11yRole,
    app::App,
    error::EngineError,
    interaction::{ActionCtx, ScrollConfig, ScrollHandle},
    math::{LogicalPoint, LogicalPx, LogicalRect, LogicalSize, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{style::StylePatch, style_builder::StyleBuilder},
    popup::{
        Anchor, DismissPolicy, FocusPolicy, InitialFocus, PlacementSpec, PopupA11y, PopupAnimation,
        PopupError, PopupSpec, PopupTier, RestoreFocus, ScrimSpec, ShiftPolicy,
    },
    text::{TextAlign, TextStyle},
    window::WindowId,
};

use crate::{
    kit::{
        alert::{Alert, AlertBuilder},
        button::{Button, stretch_button},
        icon::IconKind,
        popup_panel::{PanelWidth, PopupPanel, PopupPanelOptions, PopupSurface},
        scroll_bar::{ScrollBar, ScrollbarAxis, ScrollbarOptions},
        size::UnifiedSizable,
    },
    theme::{handle::ThemeHandle, role::Role},
};

const CONTENT_WIDTH: f32 = 384.0;

const CONTENT_GAP: f32 = 8.0;

const TITLE_SIZE: f32 = 20.0;

const VIEWPORT_MARGIN: LogicalPx = LogicalPx::new(4.0);

const SCRIM_DENSITY: f32 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DialogRole {
    Dialog,
    AlertDialog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogClose {
    Confirmed,
    Cancelled,
    Dismissed,
}

#[derive(Debug)]
pub enum DialogError {
    Popup(PopupError),
    Engine(EngineError),
    NotOpen,
}

impl From<PopupError> for DialogError {
    fn from(error: PopupError) -> Self {
        Self::Popup(error)
    }
}

impl From<EngineError> for DialogError {
    fn from(error: EngineError) -> Self {
        Self::Engine(error)
    }
}

impl fmt::Display for DialogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Popup(error) => error.fmt(formatter),
            Self::Engine(error) => error.fmt(formatter),
            Self::NotOpen => formatter.write_str("dialog popup is not open"),
        }
    }
}

impl std::error::Error for DialogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Popup(error) => Some(error),
            Self::Engine(error) => Some(error),
            Self::NotOpen => None,
        }
    }
}

pub type DialogContent = Rc<dyn Fn(&mut App, WindowId, NodeId) -> Result<(), EngineError>>;

pub type DialogAlertSource = Rc<dyn Fn(&Dialog, &ThemeHandle) -> AlertBuilder>;

pub type DialogAction = Rc<dyn Fn(&mut App)>;

pub type DialogCloseCallback = Rc<dyn Fn(DialogClose, &mut App)>;

#[derive(Debug)]
struct DialogConfig {
    title: SmolStr,
    role: DialogRole,
    width: f32,
    header: bool,
    close_button: bool,
    close_label: SmolStr,
    outside: bool,
    escape: bool,
    fade: bool,
}

impl DialogConfig {
    fn new(title: impl Into<SmolStr>) -> Self {
        Self {
            title: title.into(),
            role: DialogRole::Dialog,
            width: CONTENT_WIDTH,
            header: true,
            close_button: true,
            close_label: SmolStr::new_static("Close"),
            outside: false,
            escape: true,
            fade: true,
        }
    }

    fn a11y(&self, title: SmolStr) -> PopupA11y {
        let role = match self.role {
            DialogRole::Dialog => A11yRole::Dialog,
            DialogRole::AlertDialog => A11yRole::AlertDialog,
        };
        PopupA11y {
            role: Some(role),
            label: Some(title),
            ..PopupA11y::default()
        }
    }

    fn placement() -> PlacementSpec {
        PlacementSpec {
            gap: LogicalPx::ZERO,
            viewport_margin: VIEWPORT_MARGIN,
            shift: ShiftPolicy::None,
            ..PlacementSpec::default()
        }
    }
}

#[derive(Default)]
struct DialogState {
    window: Option<WindowId>,
    body: Option<NodeId>,
    content_root: Option<NodeId>,
    applied_width: Option<LogicalPx>,
    title_node: Option<NodeId>,
    button_row: Option<NodeId>,
    confirm_node: Option<NodeId>,
    scroll: Option<DialogScroll>,
}

#[derive(Clone)]
struct DialogScroll {
    outer: NodeId,
    content: NodeId,
    handle: ScrollHandle,
    bar: ScrollBar,
    height: LogicalPx,
}

impl DialogState {
    fn reset(&mut self) {
        self.window = None;
        self.body = None;
        self.content_root = None;
        self.applied_width = None;
        self.title_node = None;
        self.button_row = None;
        self.confirm_node = None;
        self.scroll = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseStart {
    Started,
    AlreadyClosing,
    NotOpen,
}

struct DialogInner {
    panel: PopupPanel,
    config: DialogConfig,
    theme: ThemeHandle,
    scope: SignalScope,
    title: Signal<SmolStr>,
    intent: Cell<Option<DialogClose>>,
    on_close: RefCell<Option<DialogCloseCallback>>,
    content: RefCell<Option<DialogContent>>,
    alert_source: RefCell<Option<DialogAlertSource>>,
    confirm: Option<(SmolStr, DialogAction)>,
    cancel: Option<(SmolStr, DialogAction)>,
    confirm_button: RefCell<Option<Button>>,
    cancel_button: RefCell<Option<Button>>,
    close_button: RefCell<Option<Button>>,
    alert: RefCell<Option<Alert>>,
    state: RefCell<DialogState>,
}

#[derive(Clone)]
pub struct Dialog {
    inner: Rc<DialogInner>,
}

pub struct DialogBuilder {
    config: DialogConfig,
    content: Option<DialogContent>,
    alert_source: Option<DialogAlertSource>,
    confirm: Option<(SmolStr, DialogAction)>,
    cancel: Option<(SmolStr, DialogAction)>,
    on_close: Option<DialogCloseCallback>,
}

impl DialogBuilder {
    #[must_use]
    pub fn new(title: impl Into<SmolStr>) -> Self {
        Self {
            config: DialogConfig::new(title),
            content: None,
            alert_source: None,
            confirm: None,
            cancel: None,
            on_close: None,
        }
    }

    #[must_use]
    pub fn alert(
        title: impl Into<SmolStr>,
        build: impl Fn(&Dialog, &ThemeHandle) -> AlertBuilder + 'static,
    ) -> Self {
        let mut builder = Self::new(title);
        builder.config.role = DialogRole::AlertDialog;
        builder.config.header = false;
        builder.alert_source = Some(Rc::new(build));
        builder
    }

    #[must_use]
    pub fn content(
        mut self,
        build: impl Fn(&mut App, WindowId, NodeId) -> Result<(), EngineError> + 'static,
    ) -> Self {
        self.content = Some(Rc::new(build));
        self
    }

    #[must_use]
    pub fn confirm(
        mut self,
        label: impl Into<SmolStr>,
        action: impl Fn(&mut App) + 'static,
    ) -> Self {
        self.confirm = Some((label.into(), Rc::new(action)));
        self
    }

    #[must_use]
    pub fn cancel(
        mut self,
        label: impl Into<SmolStr>,
        action: impl Fn(&mut App) + 'static,
    ) -> Self {
        self.cancel = Some((label.into(), Rc::new(action)));
        self
    }

    #[must_use]
    pub fn header(mut self, shown: bool) -> Self {
        self.config.header = shown;
        self
    }

    #[must_use]
    pub fn close_button(mut self, shown: bool) -> Self {
        self.config.close_button = shown;
        self
    }

    #[must_use]
    pub fn close_label(mut self, label: impl Into<SmolStr>) -> Self {
        self.config.close_label = label.into();
        self
    }

    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        if width.is_finite() && width > 0.0 {
            self.config.width = width;
        }
        self
    }

    #[must_use]
    pub fn outside(mut self, outside: bool) -> Self {
        self.config.outside = outside;
        self
    }

    #[must_use]
    pub fn escape(mut self, escape: bool) -> Self {
        self.config.escape = escape;
        self
    }

    #[must_use]
    pub fn fade(mut self, fade: bool) -> Self {
        self.config.fade = fade;
        self
    }

    #[must_use]
    pub fn on_close(mut self, callback: impl Fn(DialogClose, &mut App) + 'static) -> Self {
        self.on_close = Some(Rc::new(callback));
        self
    }

    #[must_use]
    pub fn build(self, app: &mut App, theme: ThemeHandle, key: impl Into<SmolStr>) -> Dialog {
        let key = key.into();
        let scope = app.signal_scope();
        let title = app.signal_scoped(scope, self.config.title.clone());
        let surface = PopupSurface::dialog();
        let panel_width = px(self.config.width + surface.padding.value() * 2.0);
        let panel = PopupPanel::new(
            app,
            theme.clone(),
            key,
            PopupPanelOptions::new(surface, PanelWidth::Exact(panel_width)),
        );
        let inner = Rc::new(DialogInner {
            panel: panel.clone(),
            config: self.config,
            theme,
            scope,
            title,
            intent: Cell::new(None),
            on_close: RefCell::new(self.on_close),
            content: RefCell::new(self.content),
            alert_source: RefCell::new(self.alert_source),
            confirm: self.confirm,
            cancel: self.cancel,
            confirm_button: RefCell::new(None),
            cancel_button: RefCell::new(None),
            close_button: RefCell::new(None),
            alert: RefCell::new(None),
            state: RefCell::new(DialogState::default()),
        });
        let weak = Rc::downgrade(&inner);
        panel.on_close(move |app, _reason| {
            if let Some(inner) = weak.upgrade() {
                let dialog = Dialog { inner };
                dialog.finish_close(app);
            }
        });
        Dialog { inner }
    }
}

impl Dialog {
    pub fn open(&self, app: &mut App, window: WindowId) -> Result<bool, DialogError> {
        if self.inner.panel.is_open(app) {
            return Ok(false);
        }
        if self.inner.panel.id().is_some() {
            self.inner.panel.close_now(app);
        }
        self.inner.intent.set(None);
        self.release_content(app);
        self.inner.state.borrow_mut().reset();
        let viewport = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .size();
        let title = app.read(&self.inner.title).clone();
        let mut spec = PopupSpec::new(
            Anchor::WindowRect(LogicalRect::from_origin_size(
                LogicalPoint::new(viewport.width * 0.5, viewport.height * 0.5),
                LogicalSize::ZERO,
            )),
            DialogConfig::placement(),
        );
        spec.tier = PopupTier::Modal;
        spec.focus = FocusPolicy::Trap {
            restore: RestoreFocus::Previous,
            initial: InitialFocus::DefaultButton,
        };
        spec.dismiss = DismissPolicy::Explicit {
            outside: self.inner.config.outside,
            escape: self.inner.config.escape,
        };
        spec.animation = if self.inner.config.fade {
            PopupAnimation::FadeScale
        } else {
            PopupAnimation::None
        };
        spec.a11y = self.inner.config.a11y(title);
        let scrim_color = self
            .inner
            .theme
            .theme_for(self.inner.theme.current(app))
            .components()
            .dialog
            .backdrop;
        spec.scrim = Some(ScrimSpec {
            density: SCRIM_DENSITY,
            color: scrim_color,
        });
        let dialog = self.clone();
        let opened = self
            .inner
            .panel
            .open(app, window, spec, move |app, window, body| {
                dialog.build_content(app, window, body)
            });
        let id = match opened {
            Ok(id) => id,
            Err(error) => {
                self.inner.panel.close_now(app);
                self.release_content(app);
                self.inner.state.borrow_mut().reset();
                return Err(error.into());
            }
        };
        if let Some(confirm) = self.inner.state.borrow().confirm_node {
            let _ = app.set_popup_initial_focus(id, confirm);
        }
        self.inner.state.borrow_mut().window = Some(window);
        if let Err(error) = self.apply_center(app, window) {
            self.inner.panel.close_now(app);
            self.release_content(app);
            self.inner.state.borrow_mut().window = None;
            self.inner.state.borrow_mut().body = None;
            return Err(error);
        }
        Ok(true)
    }

    pub fn close(&self, app: &mut App) -> bool {
        match self.begin_close(app, DialogClose::Dismissed) {
            CloseStart::Started | CloseStart::AlreadyClosing => true,
            CloseStart::NotOpen => self.inner.panel.id().is_some(),
        }
    }

    pub fn close_now(&self, app: &mut App) -> bool {
        if self.inner.panel.id().is_none() {
            return false;
        }
        self.inner.panel.close_now(app)
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, DialogError> {
        if self.inner.panel.id().is_none() {
            return Ok(false);
        }
        let mut changed = self.inner.panel.sync(app);
        if self.inner.panel.id().is_none() {
            return Ok(changed);
        }
        changed |= self.sync_width(app);
        changed |= self.sync_scroll(app);
        changed |= self.sync_scroll_bar(app)?;
        for button in [
            self.inner.confirm_button.borrow().clone(),
            self.inner.cancel_button.borrow().clone(),
            self.inner.close_button.borrow().clone(),
        ]
        .into_iter()
        .flatten()
        {
            changed |= button.sync(app)?;
        }
        if let Some(alert) = self.inner.alert.borrow().as_ref() {
            changed |= alert.sync(app)?;
        }
        Ok(changed)
    }

    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        self.inner.panel.id().is_some() || self.inner.panel.state(app).is_some()
    }

    #[must_use]
    pub fn is_closing(&self, app: &App) -> bool {
        self.inner.panel.is_closing(app)
    }

    #[must_use]
    pub fn state(&self, app: &App) -> Option<ui_core::popup::PopupState> {
        self.inner.panel.state(app)
    }

    #[must_use]
    pub fn title(&self, app: &App) -> SmolStr {
        app.read(&self.inner.title).clone()
    }

    pub fn set_title(&self, app: &mut App, title: impl Into<SmolStr>) -> bool {
        let title = title.into();
        if !app.write(&self.inner.title, title.clone()) {
            return false;
        }
        if self.inner.panel.id().is_some() {
            let _ = self
                .inner
                .panel
                .set_a11y(app, self.inner.config.a11y(title));
        }
        true
    }

    #[must_use]
    pub fn key(&self) -> &str {
        self.inner.panel.key()
    }

    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.panel.open_state()
    }

    #[must_use]
    pub fn popup_node(&self) -> Option<NodeId> {
        self.inner.panel.popup_node()
    }

    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.panel.panel_node()
    }

    #[must_use]
    pub fn body_node(&self) -> Option<NodeId> {
        self.inner.state.borrow().body
    }

    #[must_use]
    pub fn confirm_button(&self) -> Option<Button> {
        self.inner.confirm_button.borrow().clone()
    }

    #[must_use]
    pub fn cancel_button(&self) -> Option<Button> {
        self.inner.cancel_button.borrow().clone()
    }

    #[must_use]
    pub fn close_button(&self) -> Option<Button> {
        self.inner.close_button.borrow().clone()
    }

    #[must_use]
    pub fn with_alert<R>(&self, read: impl FnOnce(&Alert) -> R) -> Option<R> {
        self.inner.alert.borrow().as_ref().map(read)
    }

    pub fn release(&self, app: &mut App) {
        self.inner.panel.release(app);
        self.release_content(app);
        self.inner.intent.set(None);
        self.inner.state.borrow_mut().reset();
        let _ = app.release_scope(self.inner.scope);
    }

    fn build_content(
        &self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<(), EngineError> {
        let key = self.inner.panel.key().to_owned();
        let root = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:content"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .relative()
                    .width(px(self.inner.config.width))
                    .gap(px(CONTENT_GAP))
                    .build(),
            )
            .build(tree_mut(app, window)?, body)?;
        {
            let mut state = self.inner.state.borrow_mut();
            state.content_root = Some(root);
            state.applied_width = Some(px(self.inner.config.width));
        }
        if self.inner.config.header && !self.inner.config.title.is_empty() {
            let title_theme = self.inner.theme.clone();
            let title = self.inner.title;
            let node = NodeBuilder::text(self.inner.config.title.clone(), {
                let mut style = TextStyle::new(px(TITLE_SIZE));
                style.weight = 700;
                style.alignment = TextAlign::Center;
                style
            })
            .id(format_smolstr!("{key}:title"))
            .style(StyleBuilder::new().width_full().build())
            .bind_text(move |cx| cx.read(&title).clone())
            .bind_style(move |cx| StylePatch {
                foreground: Some(title_theme.role(cx, Role::FgDefault)),
                ..StylePatch::default()
            })
            .build(tree_mut(app, window)?, root)?;
            self.inner.state.borrow_mut().title_node = Some(node);
        }
        let scroll_outer = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:scroll"))
            .style(
                StyleBuilder::new()
                    .relative()
                    .width_full()
                    .min_width_0()
                    .min_height_0()
                    .overflow_hidden()
                    .build(),
            )
            .tab_stop(false)
            .build(tree_mut(app, window)?, root)?;
        let viewport = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:scroll:viewport"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .relative()
                    .size_full()
                    .min_width_0()
                    .min_height_0()
                    .overflow_hidden()
                    .build(),
            )
            .tab_stop(false)
            .build(tree_mut(app, window)?, scroll_outer)?;
        let content_body = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:content-body"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .flex_none()
                    .width_full()
                    .min_width_0()
                    .gap(px(CONTENT_GAP))
                    .build(),
            )
            .build(tree_mut(app, window)?, viewport)?;
        let handle = app.make_scroll(window, viewport, ScrollConfig::default())?;
        let bar = ScrollBar::new(
            app,
            self.inner.theme.clone(),
            format_smolstr!("{key}:scroll:bar"),
            ScrollbarAxis::Vertical,
            ScrollbarOptions::default().hover().tab_stop(false),
        );
        bar.build(tree_mut(app, window)?, scroll_outer)?;
        bar.attach(app, window, viewport)?;
        {
            let mut state = self.inner.state.borrow_mut();
            state.body = Some(content_body);
            state.scroll = Some(DialogScroll {
                outer: scroll_outer,
                content: content_body,
                handle,
                bar,
                height: LogicalPx::ZERO,
            });
        }
        let content = self.inner.content.borrow().clone();
        if let Some(content) = content {
            content(app, window, content_body)?;
        }
        self.build_alert(app, window, content_body)?;
        if self.inner.confirm.is_some() || self.inner.cancel.is_some() {
            self.build_button_row(app, window, root, &key)?;
        }
        if self.inner.config.close_button && matches!(self.inner.config.role, DialogRole::Dialog) {
            let weak = Rc::downgrade(&self.inner);
            let button = Button::new(
                app,
                self.inner.theme.clone(),
                format_smolstr!("{key}:close"),
            )
            .icon(IconKind::X)
            .a11y_label(self.inner.config.close_label.clone())
            .ghost()
            .small()
            .on_click(move |ctx: &mut ActionCtx<'_>| {
                if let Some(inner) = weak.upgrade() {
                    let dialog = Dialog { inner };
                    let _ = dialog.begin_close(ctx.app_mut(), DialogClose::Dismissed);
                }
            });
            let node = button.build(app, window, root)?;
            position_close_button(app, window, node)?;
            *self.inner.close_button.borrow_mut() = Some(button);
        }
        Ok(())
    }

    fn build_button_row(
        &self,
        app: &mut App,
        window: WindowId,
        root: NodeId,
        key: &str,
    ) -> Result<(), EngineError> {
        let row = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:buttons"))
            .style(StyleBuilder::new().flex_row().width_full().build())
            .build(tree_mut(app, window)?, root)?;
        self.inner.state.borrow_mut().button_row = Some(row);
        let paired = self.inner.confirm.is_some() && self.inner.cancel.is_some();
        if let Some((label, action)) = &self.inner.confirm {
            let weak = Rc::downgrade(&self.inner);
            let action = action.clone();
            let button = Button::new(app, self.inner.theme.clone(), format_smolstr!("{key}:ok"))
                .label(label.clone())
                .on_click(move |ctx: &mut ActionCtx<'_>| {
                    if confirm_started(&weak, ctx.app_mut(), DialogClose::Confirmed) {
                        action(ctx.app_mut());
                    }
                });
            let button = if paired { button.joined(0, 2) } else { button };
            let node = button.build(app, window, row)?;
            stretch_button(app, window, node)?;
            self.inner.state.borrow_mut().confirm_node = Some(node);
            *self.inner.confirm_button.borrow_mut() = Some(button);
        }
        if let Some((label, action)) = &self.inner.cancel {
            let weak = Rc::downgrade(&self.inner);
            let action = action.clone();
            let button = Button::new(
                app,
                self.inner.theme.clone(),
                format_smolstr!("{key}:cancel"),
            )
            .secondary()
            .label(label.clone())
            .joined(1, 2)
            .on_click(move |ctx: &mut ActionCtx<'_>| {
                if confirm_started(&weak, ctx.app_mut(), DialogClose::Cancelled) {
                    action(ctx.app_mut());
                }
            });
            let node = button.build(app, window, row)?;
            stretch_button(app, window, node)?;
            *self.inner.cancel_button.borrow_mut() = Some(button);
        }
        Ok(())
    }

    fn build_alert(
        &self,
        app: &mut App,
        window: WindowId,
        body: NodeId,
    ) -> Result<(), EngineError> {
        let source = self.inner.alert_source.borrow().clone();
        let Some(source) = source else {
            return Ok(());
        };
        let builder = source(self, &self.inner.theme);
        let weak = Rc::downgrade(&self.inner);
        let builder = builder.closing_guard(move |app| {
            weak.upgrade()
                .is_some_and(|inner| inner.panel.is_closing(app))
        });
        let alert = builder.build(app, window, body, &self.inner.theme)?;
        *self.inner.alert.borrow_mut() = Some(alert);
        Ok(())
    }

    fn sync_width(&self, app: &mut App) -> bool {
        let Some(available) = self.inner.panel.available_body_width(app) else {
            return false;
        };
        let (Some(window), Some(root)) = ({
            let state = self.inner.state.borrow();
            (state.window, state.content_root)
        }) else {
            return false;
        };
        let target = px(self.inner.config.width).min(available);
        if self.inner.state.borrow().applied_width == Some(target) {
            return false;
        }
        let Some(target_window) = app.window_mut(window) else {
            return false;
        };
        let Ok(node) = target_window.tree().get(root) else {
            return false;
        };
        let mut style = node.style().clone();
        style.width_length(target);
        if target_window.tree_mut().set_style(root, style).is_err() {
            return false;
        }
        self.inner.state.borrow_mut().applied_width = Some(target);
        true
    }

    fn sync_scroll(&self, app: &mut App) -> bool {
        let Some(available) = self.inner.panel.available_body_height(app) else {
            return false;
        };
        let (Some(window), Some(scroll), title, buttons) = ({
            let state = self.inner.state.borrow();
            (
                state.window,
                state.scroll.clone(),
                state.title_node,
                state.button_row,
            )
        }) else {
            return false;
        };
        let Some(target) = app.window(window) else {
            return false;
        };
        let tree = target.tree();
        let Some(content_size) = tree
            .get(scroll.content)
            .ok()
            .map(|node| node.layout().bounds.size)
        else {
            return false;
        };
        let mut fixed = LogicalPx::ZERO;
        let mut siblings = 0u32;
        for node in [title, buttons].into_iter().flatten() {
            let Ok(other) = tree.get(node) else {
                return false;
            };
            fixed += other.layout().bounds.size.height;
            siblings += 1;
        }
        let fixed = fixed + px(CONTENT_GAP * siblings as f32);
        let available_for_scroll = (available - fixed).max(px(1.0));
        let target_height = content_size.height.min(available_for_scroll);
        let _ = app.set_scroll_content(
            &scroll.handle,
            LogicalSize::new(content_size.width, content_size.height),
        );
        if (target_height - scroll.height).value().abs() < 0.01 {
            return false;
        }
        {
            let Some(target) = app.window_mut(window) else {
                return false;
            };
            let Ok(node) = target.tree().get(scroll.outer) else {
                return false;
            };
            let mut style = node.style().clone();
            style.height_length(target_height);
            let _ = target.tree_mut().set_style(scroll.outer, style);
        }
        if let Some(current) = self.inner.state.borrow_mut().scroll.as_mut() {
            current.height = target_height;
        }
        true
    }

    fn sync_scroll_bar(&self, app: &mut App) -> Result<bool, EngineError> {
        let bar = self
            .inner
            .state
            .borrow()
            .scroll
            .as_ref()
            .map(|scroll| scroll.bar.clone());
        match bar {
            Some(bar) => bar.sync(app),
            None => Ok(false),
        }
    }

    fn apply_center(&self, app: &mut App, window: WindowId) -> Result<(), DialogError> {
        let panel = self.inner.panel.panel_node().ok_or(DialogError::NotOpen)?;
        let viewport = app
            .window(window)
            .map(|target| target.size())
            .ok_or(EngineError::UnknownWindow { window })?;
        let size = measure_panel_size(app, window, panel)?;
        let desired = centered_origin(viewport, size);
        if !self.inner.panel.set_anchor(app, center_anchor(desired)) {
            return Err(DialogError::NotOpen);
        }
        Ok(())
    }

    fn begin_close(&self, app: &mut App, reason: DialogClose) -> CloseStart {
        if !self.inner.panel.is_open(app) {
            return CloseStart::NotOpen;
        }
        if self.inner.panel.is_closing(app) {
            return CloseStart::AlreadyClosing;
        }
        self.inner.intent.set(Some(reason));
        if self.inner.panel.close(app) {
            CloseStart::Started
        } else {
            CloseStart::AlreadyClosing
        }
    }

    fn finish_close(&self, app: &mut App) {
        let close = self.inner.intent.take().unwrap_or(DialogClose::Dismissed);
        let mut state = self.inner.state.borrow_mut();
        state.window = None;
        state.body = None;
        state.content_root = None;
        state.applied_width = None;
        state.title_node = None;
        state.button_row = None;
        state.confirm_node = None;
        drop(state);
        let callback = self.inner.on_close.borrow().clone();
        if let Some(callback) = callback {
            callback(close, app);
        }
    }

    fn release_content(&self, app: &mut App) {
        for button in [
            self.inner.confirm_button.borrow_mut().take(),
            self.inner.cancel_button.borrow_mut().take(),
            self.inner.close_button.borrow_mut().take(),
        ]
        .into_iter()
        .flatten()
        {
            button.release(app);
        }
        if let Some(alert) = self.inner.alert.borrow_mut().take() {
            alert.release(app);
        }
        if let Some(scroll) = self.inner.state.borrow_mut().scroll.take() {
            scroll.bar.release(app);
        }
    }
}

fn centered_origin(viewport: LogicalSize, size: LogicalSize) -> LogicalPoint {
    LogicalPoint::new(
        ((viewport.width - size.width) * 0.5).max(VIEWPORT_MARGIN),
        ((viewport.height - size.height) * 0.5).max(VIEWPORT_MARGIN),
    )
}

fn center_anchor(origin: LogicalPoint) -> Anchor {
    Anchor::WindowRect(LogicalRect::from_origin_size(origin, LogicalSize::ZERO))
}

fn confirm_started(weak: &Weak<DialogInner>, app: &mut App, reason: DialogClose) -> bool {
    match weak.upgrade() {
        Some(inner) => matches!(
            (Dialog { inner }).begin_close(app, reason),
            CloseStart::Started
        ),
        None => true,
    }
}

fn tree_mut(app: &mut App, window: WindowId) -> Result<&mut NodeTree, EngineError> {
    app.window_mut(window)
        .map(|target| target.tree_mut())
        .ok_or(EngineError::UnknownWindow { window })
}

fn measure_panel_size(
    app: &mut App,
    window: WindowId,
    panel: NodeId,
) -> Result<LogicalSize, EngineError> {
    match app.refresh_text(window) {
        Some(Ok(_)) => {}
        Some(Err(error)) => return Err(error),
        None => return Err(EngineError::UnknownWindow { window }),
    }
    app.window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .compute_layout()?;
    match app.layout_text(window) {
        Some(Ok(_)) => {}
        Some(Err(error)) => return Err(error),
        None => return Err(EngineError::UnknownWindow { window }),
    }
    app.window_mut(window)
        .ok_or(EngineError::UnknownWindow { window })?
        .compute_layout()?;
    app.window(window)
        .and_then(|target| target.tree().get(panel).ok())
        .map(|target| target.layout().bounds.size)
        .ok_or(EngineError::StaleNode { node: panel })
}

fn position_close_button(app: &mut App, window: WindowId, node: NodeId) -> Result<(), EngineError> {
    let tree = tree_mut(app, window)?;
    let mut style = tree.get(node)?.style().clone();
    style
        .position(Position::Absolute)
        .inset_top(px(0.0))
        .inset_right(px(0.0));
    tree.set_style(node, style)
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

    use smol_str::{SmolStr, format_smolstr};
    use taffy::style::Overflow;
    use ui_core::{
        a11y::A11yRole,
        app::App,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, signal::Signal},
        paint::{
            color::{Color, ColorResolver},
            primitive::{Primitive, QuadPaint},
            style_builder::{PaintBuilder, StyleBuilder},
        },
        popup::{PopupSnapshotEntry, PopupState, PopupTier},
        test_support::{app::TestApp, record::FrameRecord},
        window::{FrameOutcome, WindowId, WindowOptions},
    };

    use super::{Dialog, DialogBuilder, DialogClose};
    use crate::{
        kit::{
            alert::AlertBuilder,
            context_menu::{ContextMenu, ContextMenuOptions},
            input::{self as kit_input, InputSpec, InputState},
            menu::{MenuAction, MenuEntry},
            test_support::{a11y_of, full_a11y},
        },
        theme::{appearance::Appearance, components::ControlSize, handle::ThemeHandle},
    };

    const VIEWPORT: LogicalSize = LogicalSize::new(px(800.0), px(600.0));

    struct Harness {
        app: TestApp,
        window: WindowId,
        root: NodeId,
        theme: ThemeHandle,
    }

    impl Harness {
        fn new() -> Self {
            Self::sized(VIEWPORT)
        }

        fn sized(size: LogicalSize) -> Self {
            let app = TestApp::with_options(WindowOptions {
                size,
                ..WindowOptions::default()
            });
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let mut theme_app = app;
            let theme = ThemeHandle::new(theme_app.app_mut(), Appearance::Light);
            Self {
                app: theme_app,
                window,
                root,
                theme,
            }
        }

        fn build(&mut self, builder: DialogBuilder, key: &str) -> Dialog {
            builder.build(self.app.app_mut(), self.theme.clone(), key)
        }

        fn dialog(&mut self, builder: DialogBuilder, key: &str) -> Dialog {
            let dialog = self.build(builder, key);
            assert!(
                dialog
                    .open(self.app.app_mut(), self.window)
                    .expect("打开对话框"),
                "首次打开必须真的打开"
            );
            self.settle(&dialog);
            dialog
        }

        fn settle(&mut self, dialog: &Dialog) {
            for _ in 0..6 {
                let _ = dialog.sync(self.app.app_mut()).expect("同步");
                let _ = self.app.drain().expect("帧");
            }
            let _ = dialog.sync(self.app.app_mut()).expect("同步");
        }

        fn tick(&mut self, dialog: &Dialog, by: Duration) -> FrameRecord {
            self.app.advance(by);
            let _ = dialog.sync(self.app.app_mut()).expect("同步");
            self.app.drain().expect("帧").clone()
        }

        fn drain_until_idle(&mut self) {
            for _ in 0..4 {
                let _ = self.app.drain().expect("帧");
            }
        }

        fn settle_closed(&mut self, dialog: &Dialog) -> bool {
            for _ in 0..40 {
                self.tick(dialog, Duration::from_millis(16));
                if !dialog.is_open(self.app.app()) {
                    return true;
                }
            }
            false
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

        fn center(&self, node: NodeId) -> LogicalPoint {
            let bounds = self.bounds(node);
            LogicalPoint::new(
                bounds.origin.x + bounds.size.width * 0.5,
                bounds.origin.y + bounds.size.height * 0.5,
            )
        }

        fn click_node(&mut self, node: NodeId) {
            let point = self.center(node);
            self.click_at(point);
        }

        fn click_at(&mut self, point: LogicalPoint) {
            let _ = self.app.inject(InputEvent::pointer_down(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.inject(InputEvent::pointer_up(
                point,
                PointerButton::Primary,
                Modifiers::empty(),
            ));
            let _ = self.app.drain().expect("帧");
        }

        fn hover_at(&mut self, point: LogicalPoint) {
            let _ = self
                .app
                .inject(InputEvent::pointer_move(point, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
        }

        fn escape(&mut self) {
            let _ = self
                .app
                .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
        }

        fn tab(&mut self) {
            let _ = self
                .app
                .inject(InputEvent::key_down(KeyCode::Tab, Modifiers::empty()));
            let _ = self.app.drain().expect("帧");
        }

        fn tree_has(&self, node: NodeId) -> bool {
            self.app
                .app()
                .window(self.window)
                .expect("窗口")
                .tree()
                .get(node)
                .is_ok()
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

        fn text_of(&self, node: NodeId) -> Option<SmolStr> {
            let tree = self.app.app().window(self.window).expect("窗口").tree();
            match tree.get(node).expect("文本节点").kind() {
                ui_core::node::tree::NodeKind::Text(content) => Some(content.text.clone()),
                other => panic!("期望文本节点，实际 {other:?}"),
            }
        }

        fn background_with_actions(&mut self) -> (NodeId, Signal<u32>) {
            let clicks = self.app.app_mut().signal(0u32);
            let action = Action::Custom(format_smolstr!("background.click"));
            let node = {
                let tree = self
                    .app
                    .app_mut()
                    .window_mut(self.window)
                    .expect("窗口")
                    .tree_mut();
                NodeBuilder::boxed()
                    .id("background")
                    .layout_override(LogicalRect::new(
                        px(0.0),
                        px(0.0),
                        VIEWPORT.width,
                        VIEWPORT.height,
                    ))
                    .paint(PaintBuilder::new().background(Color::WHITE).build())
                    .on_click(action.clone())
                    .build(tree, self.root)
                    .expect("背景")
            };
            let clicks_for_handler = clicks;
            self.app.app_mut().on_action(action, move |ctx| {
                ctx.update(&clicks_for_handler, |count| *count += 1);
            });
            (node, clicks)
        }

        fn focused(&self) -> Option<NodeId> {
            self.app.app().focused(self.window)
        }

        fn stack(&self) -> ui_core::popup::PopupStackSnapshot {
            self.app
                .app()
                .popup_stack(self.window)
                .expect("每个窗口都有 popup 栈")
        }

        fn entry_of(&self, dialog: &Dialog) -> PopupSnapshotEntry {
            let node = dialog.popup_node().expect("浮层内容节点");
            self.stack()
                .entries
                .into_iter()
                .find(|entry| entry.node == node)
                .expect("对话框必须在 popup 栈里")
        }

        fn scrim_rect(&self) -> Option<LogicalRect> {
            let scrim = self
                .stack()
                .entries
                .into_iter()
                .find(|entry| entry.tier == PopupTier::Scrim)?;
            Some(self.bounds(scrim.node))
        }

        fn scrim_node(&self) -> Option<NodeId> {
            self.stack()
                .entries
                .into_iter()
                .find(|entry| entry.tier == PopupTier::Scrim)
                .map(|entry| entry.node)
        }

        fn modal_entries(&self) -> Vec<PopupSnapshotEntry> {
            self.stack()
                .entries
                .into_iter()
                .filter(|entry| entry.focus.is_modal())
                .collect()
        }
    }

    fn content_box(
        width: f32,
        height: f32,
    ) -> impl Fn(&mut App, WindowId, NodeId) -> Result<(), ui_core::error::EngineError> {
        let tracked = Rc::new(std::cell::RefCell::new(None));
        content_box_tracked(width, height, tracked)
    }

    fn content_box_tracked(
        width: f32,
        height: f32,
        tracked: Rc<std::cell::RefCell<Option<NodeId>>>,
    ) -> impl Fn(&mut App, WindowId, NodeId) -> Result<(), ui_core::error::EngineError> {
        move |app: &mut App, window: WindowId, parent: NodeId| {
            let tree = app
                .window_mut(window)
                .ok_or(ui_core::error::EngineError::UnknownWindow { window })?
                .tree_mut();
            let node = NodeBuilder::boxed()
                .id("body-box")
                .style(
                    StyleBuilder::new()
                        .width(px(width))
                        .height(px(height))
                        .build(),
                )
                .paint(PaintBuilder::new().background(Color::WHITE).build())
                .build(tree, parent)?;
            *tracked.borrow_mut() = Some(node);
            Ok(())
        }
    }

    fn focusable_box()
    -> impl Fn(&mut App, WindowId, NodeId) -> Result<(), ui_core::error::EngineError> {
        move |app: &mut App, window: WindowId, parent: NodeId| {
            let action = Action::Custom(SmolStr::new_static("focusable.box"));
            let tree = app
                .window_mut(window)
                .ok_or(ui_core::error::EngineError::UnknownWindow { window })?
                .tree_mut();
            NodeBuilder::boxed()
                .id("focusable-box")
                .style(StyleBuilder::new().size(px(60.0), px(20.0)).build())
                .tab_stop(true)
                .on_click(action)
                .build(tree, parent)
                .map(|_| ())
        }
    }

    fn color_alpha(color: Color) -> f32 {
        match color {
            Color::Srgb { alpha, .. }
            | Color::LinearSrgb { alpha, .. }
            | Color::DisplayP3 { alpha, .. }
            | Color::Oklab { alpha, .. }
            | Color::Oklch { alpha, .. } => alpha,
        }
    }

    fn quad_alpha(app: &App, window: WindowId, bounds: LogicalRect) -> Option<f32> {
        app.window(window)?
            .scene()
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) if quad.bounds == bounds => match &quad.paint {
                    QuadPaint::Solid { fill, .. } => Some(color_alpha(*fill)),
                    QuadPaint::Gradient(_) => None,
                },
                _ => None,
            })
    }

    fn first_glyph_alpha(app: &App, window: WindowId) -> Option<f32> {
        app.window(window)?
            .scene()
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::GlyphRun(run) => Some(color_alpha(run.color)),
                _ => None,
            })
    }

    fn record_close(flag: Rc<Cell<Option<DialogClose>>>) -> impl Fn(DialogClose, &mut App) {
        move |reason, _app| flag.set(Some(reason))
    }

    #[test]
    fn the_panel_is_centered_in_the_viewport() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Centered")
                .fade(false)
                .content(content_box(120.0, 40.0)),
            "centered",
        );
        let panel = dialog.panel_node().expect("面板");
        let bounds = harness.bounds(panel);
        assert!(
            (bounds.origin.x - (VIEWPORT.width - bounds.size.width) * 0.5).abs() < px(0.5),
            "面板水平居中：{bounds:?}"
        );
        assert!(
            (bounds.origin.y - (VIEWPORT.height - bounds.size.height) * 0.5).abs() < px(0.5),
            "面板垂直居中：{bounds:?}"
        );
        assert_eq!(
            bounds.size.width,
            px(384.0 + 16.0),
            "面板宽度 = 内容宽度 384 + 两侧各 8 px 内边距"
        );
        assert_eq!(
            harness.entry_of(&dialog).hit_rect,
            bounds,
            "popup hit rect = 面板布局盒"
        );
    }

    #[test]
    fn resizing_the_window_keeps_the_anchor_opened_with() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(DialogBuilder::new("Anchor").fade(false), "anchor");
        let panel = dialog.panel_node().expect("面板");
        let before = harness.bounds(panel);
        assert_eq!(
            before.origin,
            harness.entry_of(&dialog).hit_rect.origin,
            "打开时面板原点 = 锚点求解结果"
        );
        harness
            .app
            .app_mut()
            .window_mut(harness.window)
            .expect("窗口")
            .set_size(LogicalSize::new(px(700.0), px(500.0)));
        harness.tick(&dialog, Duration::from_millis(40));
        harness.tick(&dialog, Duration::from_millis(40));
        let after = harness.bounds(panel);
        assert_eq!(
            after.origin, before.origin,
            "resize 不得重新居中：居中只由 open 时的 anchor 决定"
        );
        assert_eq!(
            harness.entry_of(&dialog).hit_rect,
            after,
            "resize 后 popup rect 仍等于面板布局盒"
        );
        let centered_x = (700.0 - after.size.width.value()) * 0.5;
        let centered_y = (500.0 - after.size.height.value()) * 0.5;
        assert!(
            (after.origin.x.value() - centered_x).abs() > 1.0
                || (after.origin.y.value() - centered_y).abs() > 1.0,
            "resize 后不得回到新视口中心（recenter 已删除）：{after:?} 中心 ({centered_x:.1}, \
             {centered_y:.1})"
        );
        assert!(dialog.is_open(harness.app.app()), "resize 不得关闭对话框");
        assert_eq!(dialog.state(harness.app.app()), Some(PopupState::Open));
    }

    #[test]
    fn the_first_visible_frame_has_the_settled_rect() {
        for fade in [false, true] {
            let mut harness = Harness::new();
            let dialog = harness.build(
                DialogBuilder::new("First frame")
                    .fade(fade)
                    .content(content_box(120.0, 40.0)),
                "first-frame",
            );
            assert!(
                dialog
                    .open(harness.app.app_mut(), harness.window)
                    .expect("打开"),
                "首次打开"
            );
            harness.app.drain().expect("首帧");
            let first = dialog
                .panel_node()
                .and_then(|panel| harness.app.probe().bounds(panel))
                .expect("首帧面板");
            assert_eq!(
                dialog.state(harness.app.app()),
                Some(PopupState::Open),
                "首帧即 Open（不再等 reveal）"
            );
            for _ in 0..6 {
                let _ = dialog.sync(harness.app.app_mut()).expect("同步");
                harness.app.drain().expect("帧");
            }
            let stable = harness.bounds(dialog.panel_node().expect("面板"));
            assert_eq!(first, stable, "fade={fade}: 首可见帧 rect 必须等于稳定帧");
            assert_eq!(
                harness.entry_of(&dialog).hit_rect,
                stable,
                "popup rect 跟随稳定后的面板"
            );
        }
    }

    #[test]
    fn the_modal_dialog_degrades_fade_scale_to_fade_only() {
        let mut harness = Harness::new();
        let dialog = harness.build(
            DialogBuilder::new("Fade scale").content(content_box(120.0, 40.0)),
            "fade-scale",
        );
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("打开"),
            "首次打开"
        );
        harness.app.drain().expect("首帧");
        let opening = harness.entry_of(&dialog);
        assert_eq!(
            opening.paint_scale, 1.0,
            "modal 的 FadeScale 退化为 fade-only：只淡入不缩放"
        );
        let first = opening.hit_rect;
        harness.settle(&dialog);
        let settled = harness.entry_of(&dialog);
        assert_eq!(settled.paint_scale, 1.0);
        assert_eq!(settled.state, PopupState::Open);
        assert_eq!(settled.hit_rect, first, "FadeScale 不改变 settle rect");
        assert_eq!(
            harness.bounds(dialog.panel_node().expect("面板")),
            first,
            "面板布局盒始终等于 popup rect"
        );
    }

    #[test]
    fn opening_builds_the_title_body_and_button_row() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Save changes")
                .fade(false)
                .content(content_box(100.0, 30.0))
                .confirm("Save", |_| {})
                .cancel("Cancel", |_| {}),
            "layout",
        );
        assert_eq!(dialog.title(harness.app.app()), "Save changes");
        let panel = harness.bounds(dialog.panel_node().expect("面板"));
        let body = harness.bounds(dialog.body_node().expect("内容列"));
        let ok = harness.bounds(
            dialog
                .confirm_button()
                .expect("确认按钮")
                .node()
                .expect("ok 节点"),
        );
        let cancel = harness.bounds(
            dialog
                .cancel_button()
                .expect("取消按钮")
                .node()
                .expect("cancel 节点"),
        );
        let close = harness.bounds(
            dialog
                .close_button()
                .expect("关闭按钮")
                .node()
                .expect("close 节点"),
        );
        for (name, bounds) in [("内容列", body), ("确认", ok), ("取消", cancel)] {
            assert!(
                bounds.origin.x >= panel.origin.x && bounds.max().x <= panel.max().x + 0.5,
                "{name} 必须在面板内：panel={panel:?} {name}={bounds:?}"
            );
        }
        assert!(
            ok.size.width > px(0.0)
                && cancel.size.width > px(0.0)
                && ok.origin.y == cancel.origin.y,
            "按钮行上的两个按钮等分同一行"
        );
        assert!(
            (ok.size.width - cancel.size.width).abs() < px(0.5),
            "确认 / 取消必须等分按钮行：{ok:?} / {cancel:?}"
        );
        assert!(
            close.origin.x > panel.origin.x + panel.size.width * 0.5,
            "关闭按钮在内容列右上角：{close:?}"
        );
        assert_eq!(body.size.height, px(30.0), "内容列高度 = 自定义内容高度");
        assert!(
            harness.find("layout:content").is_some(),
            "内容根保持 {{key}}:content 命名"
        );
        assert!(
            harness.find("layout:panel").is_some(),
            "面板保持 {{key}}:panel 命名"
        );
    }

    #[test]
    fn opening_a_dialog_takes_focus_and_blocks_the_background() {
        let mut harness = Harness::new();
        let (background, clicks) = harness.background_with_actions();
        let trigger_action = Action::Custom(format_smolstr!("trigger.click"));
        let trigger = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("trigger")
                .layout_override(LogicalRect::new(px(10.0), px(10.0), px(100.0), px(40.0)))
                .on_click(trigger_action)
                .build(tree, harness.root)
                .expect("触发器")
        };
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(trigger));
        let dialog = harness.dialog(
            DialogBuilder::new("Modal")
                .fade(false)
                .confirm("OK", |_| {})
                .content(content_box(80.0, 20.0)),
            "modal",
        );
        let entry = harness.entry_of(&dialog);
        assert_eq!(entry.tier, PopupTier::Modal, "对话框是 Modal tier");
        assert!(entry.focus.is_modal(), "FocusPolicy::Trap 是唯一模态真值");
        assert_eq!(
            harness.scrim_rect(),
            Some(LogicalRect::from_origin_size(LogicalPoint::ZERO, VIEWPORT)),
            "引擎遮罩覆盖整个视口"
        );
        assert_eq!(
            harness.focused(),
            dialog.confirm_button().and_then(|button| button.node()),
            "初始焦点 = 默认按钮"
        );
        harness.hover_at(LogicalPoint::new(px(30.0), px(30.0)));
        assert_ne!(
            harness.app.app().hovered(harness.window),
            Some(background),
            "模态期间背景 hover 被阻断"
        );
        harness.click_at(LogicalPoint::new(px(20.0), px(20.0)));
        assert_eq!(
            *harness.app.app().read(&clicks),
            0,
            "模态必须消费背景上的按下"
        );
        assert!(
            dialog.is_open(harness.app.app()),
            "外部按下不关闭默认对话框"
        );
    }

    #[test]
    fn confirm_runs_its_action_and_reports_confirmed() {
        let mut harness = Harness::new();
        let clicks = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Confirm")
                .fade(false)
                .confirm("OK", {
                    let clicks = clicks.clone();
                    move |_| clicks.set(clicks.get() + 1)
                })
                .on_close(record_close(reason.clone())),
            "confirm",
        );
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert_eq!(clicks.get(), 1, "确认按钮必须跑它的回调");
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()), "fade=false 立即关闭");
        assert_eq!(reason.get(), Some(DialogClose::Confirmed), "关闭原因");
        assert!(dialog.popup_node().is_none(), "关闭后浮层节点卸载");
        assert!(harness.stack().entries.is_empty(), "关闭后不残留栈项");
    }

    #[test]
    fn cancel_runs_its_action_and_reports_cancelled() {
        let mut harness = Harness::new();
        let clicks = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Cancel")
                .fade(false)
                .confirm("OK", |_| panic!("取消不得跑确认回调"))
                .cancel("Cancel", {
                    let clicks = clicks.clone();
                    move |_| clicks.set(clicks.get() + 1)
                })
                .on_close(record_close(reason.clone())),
            "cancel",
        );
        let cancel = dialog
            .cancel_button()
            .expect("取消按钮")
            .node()
            .expect("节点");
        harness.click_node(cancel);
        assert_eq!(clicks.get(), 1);
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Cancelled));
    }

    #[test]
    fn the_close_button_dismisses_without_running_confirm_or_cancel() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Close")
                .fade(false)
                .confirm("OK", |_| panic!("关闭按钮不得跑确认回调"))
                .cancel("Cancel", |_| panic!("关闭按钮不得跑取消回调"))
                .on_close(record_close(reason.clone())),
            "close-button",
        );
        let close = dialog
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("节点");
        harness.click_node(close);
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn escape_fades_the_dialog_out_and_reports_dismissed() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Escape").on_close(record_close(reason.clone())),
            "escape",
        );
        let node = dialog.popup_node().expect("浮层节点");
        harness.escape();
        assert_eq!(
            dialog.state(harness.app.app()),
            Some(PopupState::Closing),
            "Esc 进入关闭事务（Closing 淡出期间不可命中）"
        );
        assert!(
            !dialog
                .state(harness.app.app())
                .is_some_and(PopupState::is_dismissable),
            "Closing 不再可作为 dismiss 目标"
        );
        assert!(harness.tree_has(node), "淡出期间节点仍在");
        assert!(dialog.is_closing(harness.app.app()));
        assert!(harness.settle_closed(&dialog), "淡出必须完成");
        assert_eq!(
            reason.get(),
            Some(DialogClose::Dismissed),
            "Esc 是 Dismissed"
        );
        assert!(harness.modal_entries().is_empty(), "关闭后不再有模态作用域");
        assert!(harness.scrim_node().is_none(), "遮罩与模态同生命周期回收");
    }

    #[test]
    fn a_press_outside_neither_closes_nor_reaches_the_background() {
        let mut harness = Harness::new();
        let (_background, clicks) = harness.background_with_actions();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Explicit")
                .fade(false)
                .on_close(record_close(reason.clone())),
            "explicit",
        );
        harness.click_at(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(
            dialog.is_open(harness.app.app()),
            "Explicit{{outside:false}} 不得被外部点击关闭"
        );
        assert_eq!(
            *harness.app.app().read(&clicks),
            0,
            "模态必须消费背景上的按下"
        );
        assert_eq!(reason.get(), None, "没关闭就没有关闭回调");
    }

    #[test]
    fn an_outside_click_policy_closes_the_dialog() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Outside")
                .fade(false)
                .outside(true)
                .on_close(record_close(reason.clone())),
            "outside",
        );
        harness.click_at(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(
            !dialog.is_open(harness.app.app()),
            "outside=true 点遮罩即关闭"
        );
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn escape_can_be_disabled() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("No escape").fade(false).escape(false),
            "no-escape",
        );
        harness.escape();
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(
            dialog.is_open(harness.app.app()),
            "escape=false 时 Esc 不得关闭对话框"
        );
    }

    #[test]
    fn the_initial_focus_is_the_default_button() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Focus")
                .fade(false)
                .confirm("OK", |_| {})
                .cancel("Cancel", |_| {})
                .content(content_box(80.0, 20.0)),
            "focus",
        );
        let ok = dialog.confirm_button().expect("确认").node().expect("节点");
        assert_eq!(harness.focused(), Some(ok), "初始焦点必须是默认按钮");
    }

    #[test]
    fn the_initial_focus_falls_back_to_the_first_focusable() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Focus fallback")
                .fade(false)
                .close_button(false)
                .content(focusable_box()),
            "focus-fallback",
        );
        let expected = harness.find("focusable-box").expect("可聚焦盒");
        assert_eq!(
            harness.focused(),
            Some(expected),
            "没有按钮时初始焦点 = 第一个可聚焦节点"
        );
        let _ = dialog;
    }

    #[test]
    fn the_initial_focus_falls_back_to_the_container() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Focus container")
                .fade(false)
                .close_button(false)
                .content(content_box(80.0, 20.0)),
            "focus-container",
        );
        assert_eq!(
            harness.focused(),
            dialog.popup_node(),
            "无任何可聚焦节点时焦点落在浮层内容容器"
        );
    }

    #[test]
    fn tab_cycles_within_the_dialog() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Tab")
                .fade(false)
                .confirm("OK", |_| {})
                .cancel("Cancel", |_| {}),
            "tab",
        );
        let ok = dialog.confirm_button().expect("确认").node().expect("节点");
        let cancel = dialog.cancel_button().expect("取消").node().expect("节点");
        let close = dialog.close_button().expect("关闭").node().expect("节点");
        assert_eq!(harness.focused(), Some(ok), "起点 = 默认按钮");
        harness.tab();
        assert_eq!(harness.focused(), Some(cancel), "Tab 进入取消");
        harness.tab();
        assert_eq!(harness.focused(), Some(close), "Tab 进入关闭");
        harness.tab();
        assert_eq!(harness.focused(), Some(ok), "Tab 闭环回默认按钮");
    }

    #[test]
    fn closing_restores_focus_exactly_once() {
        let mut harness = Harness::new();
        let trigger = {
            let tree = harness
                .app
                .app_mut()
                .window_mut(harness.window)
                .expect("窗口")
                .tree_mut();
            NodeBuilder::boxed()
                .id("trigger")
                .layout_override(LogicalRect::new(px(10.0), px(10.0), px(100.0), px(40.0)))
                .tab_stop(true)
                .on_click(Action::Custom(format_smolstr!("trigger.click")))
                .build(tree, harness.root)
                .expect("触发器")
        };
        harness
            .app
            .app_mut()
            .set_focus(harness.window, Some(trigger));
        let dialog = harness.dialog(
            DialogBuilder::new("Restore")
                .fade(false)
                .confirm("OK", |_| {}),
            "restore",
        );
        assert_ne!(harness.focused(), Some(trigger), "打开时焦点被取走");
        assert!(dialog.close_now(harness.app.app_mut()));
        assert_eq!(
            harness.focused(),
            Some(trigger),
            "关闭后焦点必须还给之前的焦点"
        );
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let _ = harness.app.drain().expect("帧");
        assert_eq!(
            harness.focused(),
            Some(trigger),
            "恢复只发生一次，后续帧不抢焦点"
        );
    }

    #[test]
    fn the_scrim_is_hidden_and_bound_to_the_modal_lifecycle() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(DialogBuilder::new("Scrim").fade(false), "scrim");
        let scrim = harness.scrim_node().expect("引擎遮罩");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(scrim).expect("遮罩 a11y 节点");
        assert!(record.hidden, "遮罩必须 a11y hidden");
        assert_eq!(
            snapshot.nodes().iter().filter(|node| node.modal).count(),
            1,
            "恰好一个模态 a11y 节点"
        );
        assert!(dialog.close_now(harness.app.app_mut()));
        assert!(harness.scrim_node().is_none(), "模态关闭收回遮罩");
    }

    #[test]
    fn the_scrim_uses_the_theme_backdrop_color_instead_of_opaque_black() {
        let mut harness = Harness::new();
        let _dialog = harness.dialog(DialogBuilder::new("Scrim color").fade(false), "scrim-color");
        let scrim = harness.scrim_rect().expect("遮罩矩形");
        let alpha = quad_alpha(harness.app.app(), harness.window, scrim).expect("遮罩 quad");
        let backdrop = harness
            .theme
            .theme_for(Appearance::Light)
            .components()
            .dialog
            .backdrop;
        let expected = ColorResolver.resolve(backdrop).components[3];
        assert!(
            (alpha - expected).abs() < 0.01,
            "遮罩必须使用主题 backdrop 的 alpha {expected}，实际 {alpha}"
        );
        assert!(alpha < 0.9, "遮罩不得是 100% 不透明黑");
    }

    #[test]
    fn a_content_button_before_the_footer_does_not_take_the_default_focus() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Focus")
                .fade(false)
                .content(focusable_box())
                .confirm("确定", |_| {})
                .cancel("取消", |_| {}),
            "focus",
        );
        let confirm = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("确认按钮节点");
        assert_eq!(
            harness.focused(),
            Some(confirm),
            "默认焦点必须是 Dialog 的确认按钮，内容里的第一个可聚焦控件不得抢占"
        );
    }

    #[test]
    fn the_dialog_panel_draws_its_shadow_outside_the_panel() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(DialogBuilder::new("Shadow").fade(false), "shadow");
        harness.tick(&dialog, Duration::from_millis(200));
        let panel = dialog.panel_node().expect("面板");
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
            .expect("对话框面板阴影必须被绘制");
        assert!(
            shadow.blur > px(0.0) && shadow.offset.y > px(0.0),
            "对话框阴影必须带模糊与向下偏移"
        );
        let content = dialog.popup_node().expect("浮层内容根");
        let overflow = window
            .tree()
            .get(content)
            .expect("内容根节点")
            .style()
            .inner()
            .overflow;
        assert_eq!(
            (overflow.x, overflow.y),
            (Overflow::Visible, Overflow::Visible),
            "popup 内容根不得裁剪阴影"
        );
    }

    #[test]
    fn a_tall_dialog_scrolls_its_content_and_keeps_the_footer_reachable() {
        let mut harness = Harness::sized(LogicalSize::new(px(320.0), px(220.0)));
        let tracked = Rc::new(std::cell::RefCell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Tall")
                .fade(false)
                .width(280.0)
                .content(content_box_tracked(240.0, 480.0, tracked.clone()))
                .confirm("确定", |_| {})
                .cancel("取消", |_| {}),
            "tall",
        );
        let panel = harness.screen_bounds(dialog.panel_node().expect("面板"));
        assert!(
            panel.max().y <= px(216.5),
            "面板必须被视口夹住并留在边距内：{panel:?}"
        );
        assert!(
            panel.size.height < px(560.0),
            "面板必须收缩到可用高度：{panel:?}"
        );
        let confirm = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("确认按钮节点");
        let confirm_before = harness.screen_bounds(confirm);
        assert!(
            confirm_before.origin.y >= panel.origin.y - 0.5
                && confirm_before.max().y <= panel.max().y + 0.5,
            "footer 必须留在面板内：{confirm_before:?} panel={panel:?}"
        );
        let content = tracked.borrow().expect("内容节点");
        let content_before = harness.screen_bounds(content);
        let viewport = harness.find("tall:scroll:viewport").expect("滚动视口");
        let handle = harness
            .app
            .app()
            .scroll_handle(harness.window, viewport)
            .expect("滚动句柄");
        let state = harness.app.app().scroll_state(&handle).expect("滚动状态");
        assert!(
            state.content.height > state.viewport.height,
            "高内容必须内部滚动：{state:?}"
        );
        harness
            .app
            .app_mut()
            .scroll_to(&handle, LogicalPoint::new(px(0.0), px(200.0)))
            .expect("滚动");
        harness.settle(&dialog);
        assert_ne!(
            harness.screen_bounds(content),
            content_before,
            "内容必须随滚动移动"
        );
        assert_eq!(
            harness.screen_bounds(confirm),
            confirm_before,
            "footer 不随内容滚动"
        );
    }

    #[test]
    fn a_narrow_viewport_clamps_the_default_dialog_width_and_keeps_the_buttons_reachable() {
        let mut harness = Harness::sized(LogicalSize::new(px(320.0), px(220.0)));
        let dialog = harness.dialog(
            DialogBuilder::new("Narrow")
                .fade(false)
                .confirm("确定", |_| {})
                .cancel("取消", |_| {}),
            "narrow",
        );
        let panel = harness.screen_bounds(dialog.panel_node().expect("面板"));
        assert!(
            panel.origin.x >= px(0.0) && panel.max().x <= px(320.5),
            "面板不得横向超出视口：{panel:?}"
        );
        assert!(
            panel.size.width <= px(312.5),
            "默认 384 宽必须 clamp 到视口减边距：{panel:?}"
        );
        let content_root = dialog.inner.state.borrow().content_root.expect("内容根");
        let content = harness.screen_bounds(content_root);
        assert!(
            (content.size.width - (panel.size.width - px(16.0))).abs() < px(0.5),
            "内容/滚动区宽度必须随面板同步：content={content:?} panel={panel:?}"
        );
        assert!(
            content.origin.x >= panel.origin.x - 0.5 && content.max().x <= panel.max().x + 0.5,
            "内容不得横向超出面板：content={content:?} panel={panel:?}"
        );
        let confirm = harness.screen_bounds(
            dialog
                .confirm_button()
                .expect("确认按钮")
                .node()
                .expect("确认按钮节点"),
        );
        assert!(
            confirm.origin.x >= panel.origin.x - 0.5 && confirm.max().x <= panel.max().x + 0.5,
            "confirm 必须完整落在面板内：confirm={confirm:?} panel={panel:?}"
        );
        assert!(confirm.size.width > px(0.0), "confirm 不得被压成零宽");
    }

    #[test]
    fn the_dialog_panel_fades_with_its_content() {
        let mut harness = Harness::new();
        let (_background, _clicks) = harness.background_with_actions();
        let tracked = Rc::new(std::cell::RefCell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Fade")
                .content(content_box_tracked(120.0, 40.0, tracked.clone()))
                .close_button(false),
            "fade",
        );
        harness.tick(&dialog, Duration::from_millis(200));
        let panel = harness.bounds(dialog.panel_node().expect("面板"));
        let content_node = tracked.borrow().expect("内容盒节点");
        let body = harness.bounds(content_node);
        let full_panel = quad_alpha(harness.app.app(), harness.window, panel).expect("面板背景");
        let full_body = quad_alpha(harness.app.app(), harness.window, body).expect("内容盒");
        let full_title = first_glyph_alpha(harness.app.app(), harness.window).expect("标题字形");
        assert!((full_panel - 1.0).abs() < 0.01, "淡入完成后面板不透明");
        assert!((full_body - 1.0).abs() < 0.01, "淡入完成后内容不透明");

        assert!(dialog.close(harness.app.app_mut()), "开始退场");
        harness.tick(&dialog, Duration::from_millis(45));
        let panel_progress =
            quad_alpha(harness.app.app(), harness.window, panel).expect("面板背景");
        let body_progress = quad_alpha(harness.app.app(), harness.window, body).expect("内容盒");
        let title_progress =
            first_glyph_alpha(harness.app.app(), harness.window).expect("标题字形");
        assert!(
            panel_progress < full_panel * 0.99,
            "退场中面板必须变淡：{panel_progress} / {full_panel}"
        );
        assert!(
            (body_progress / full_body - panel_progress / full_panel).abs() < 0.02,
            "内容子节点必须跟面板同比例变淡（body {body_progress} / panel {panel_progress}）"
        );
        assert!(
            (title_progress / full_title - panel_progress / full_panel).abs() < 0.02,
            "文字也必须跟组淡出（title {title_progress} / panel {panel_progress}）"
        );
    }

    #[test]
    fn a_finished_fade_unloads_the_panel_in_the_same_frame() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("E10").on_close(record_close(reason.clone())),
            "e10",
        );
        let node = dialog.popup_node().expect("浮层节点");
        harness.tick(&dialog, Duration::from_millis(200));
        assert!(dialog.close(harness.app.app_mut()), "开始退场");
        let mut unloaded = false;
        for _ in 0..24 {
            harness.tick(&dialog, Duration::from_millis(16));
            if !harness.tree_has(node) {
                unloaded = true;
                break;
            }
        }
        assert!(unloaded, "淡出到终值时关闭事务必须在同一帧卸载浮层节点");
        assert!(
            dialog.is_open(harness.app.app()),
            "卸载发生在帧内；kit 状态在下一次 sync 才收尾"
        );
        assert_eq!(reason.get(), None, "收尾前不派发关闭回调");
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn the_popup_declares_the_dialog_role_label_and_modal() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(DialogBuilder::new("Delete file").fade(false), "a11y");
        let wrapper = dialog.popup_node().expect("浮层内容节点");
        let panel = dialog.panel_node().expect("面板");
        let scrim = harness.scrim_node().expect("遮罩");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(wrapper).expect("浮层 a11y");
        assert_eq!(record.role, A11yRole::Dialog, "显式 Dialog 角色");
        assert_eq!(record.label_text(), Some("Delete file"), "可访问名 = 标题");
        assert!(record.modal, "模态对话框必须声明 modal");
        assert_eq!(
            snapshot.node_for(panel).expect("面板 a11y").role,
            A11yRole::GenericContainer,
            "面板不得重复声明 Dialog"
        );
        assert!(
            snapshot.node_for(scrim).expect("遮罩 a11y").hidden,
            "遮罩必须 hidden（纯装饰）"
        );
    }

    #[test]
    fn setting_the_title_rewrites_the_title_node_and_the_a11y_name() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(DialogBuilder::new("Save changes").fade(false), "titled");
        let title_node = harness.find("titled:title").expect("标题节点");
        assert_eq!(
            harness.text_of(title_node),
            Some(SmolStr::new_static("Save changes"))
        );
        let wrapper = dialog.popup_node().expect("浮层内容节点");
        {
            let snapshot = full_a11y(&mut harness.app);
            assert_eq!(
                a11y_of(&snapshot, wrapper).label_text(),
                Some("Save changes")
            );
        }

        assert_eq!(dialog.title(harness.app.app()), "Save changes");
        assert!(dialog.set_title(harness.app.app_mut(), "保存更改"));
        assert_eq!(dialog.title(harness.app.app()), "保存更改");
        assert!(
            !dialog.set_title(harness.app.app_mut(), "保存更改"),
            "同值写入是 no-op"
        );
        harness.settle(&dialog);
        assert_eq!(
            harness.find("titled:title"),
            Some(title_node),
            "标题节点不重建"
        );
        assert_eq!(
            harness.text_of(title_node),
            Some(SmolStr::new_static("保存更改"))
        );
        let snapshot = full_a11y(&mut harness.app);
        assert_eq!(
            a11y_of(&snapshot, dialog.popup_node().expect("浮层内容节点")).label_text(),
            Some("保存更改"),
            "a11y 名跟随标题"
        );

        assert!(dialog.close_now(harness.app.app_mut()));
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("重开")
        );
        harness.settle(&dialog);
        let reopened = harness.find("titled:title").expect("重开后的标题节点");
        assert_eq!(
            harness.text_of(reopened),
            Some(SmolStr::new_static("保存更改"))
        );
        let snapshot = full_a11y(&mut harness.app);
        assert_eq!(
            a11y_of(&snapshot, dialog.popup_node().expect("浮层内容节点")).label_text(),
            Some("保存更改")
        );
    }

    #[test]
    fn setting_the_title_of_an_alert_updates_its_a11y_name() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(
            DialogBuilder::alert("Rename", |_dialog, _theme| {
                AlertBuilder::new().ok_cancel("Rename", "Enter a name.")
            })
            .fade(false),
            "alert-title",
        );
        let wrapper = dialog.popup_node().expect("浮层内容节点");
        assert!(
            harness.find("alert-title:title").is_none(),
            "alert 形状没有标题行节点"
        );
        assert!(dialog.set_title(harness.app.app_mut(), "重命名"));
        let snapshot = full_a11y(&mut harness.app);
        assert_eq!(a11y_of(&snapshot, wrapper).label_text(), Some("重命名"));
    }

    #[test]
    fn an_alert_dialog_declares_the_alert_dialog_role() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(
            DialogBuilder::alert("Delete file", |_dialog, _theme| {
                AlertBuilder::new().ok_cancel("Delete file", "This cannot be undone.")
            })
            .fade(false),
            "alert-role",
        );
        let wrapper = dialog.popup_node().expect("浮层内容节点");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(wrapper).expect("浮层 a11y");
        assert_eq!(record.role, A11yRole::AlertDialog);
        assert_eq!(record.label_text(), Some("Delete file"));
        assert!(record.modal);
        assert!(
            dialog
                .with_alert(|alert| alert.title().to_owned())
                .is_some()
        );
    }

    #[test]
    fn an_alert_content_panel_is_mounted_and_its_ok_action_can_close_the_dialog() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let ok_runs = Rc::new(Cell::new(0u32));
        let ok_runs_for_builder = ok_runs.clone();
        let dialog = harness.dialog(
            DialogBuilder::alert("Delete file", move |dialog, _theme| {
                let dialog = dialog.clone();
                let ok_runs = ok_runs_for_builder.clone();
                AlertBuilder::new()
                    .ok_cancel("Delete file", "This cannot be undone.")
                    .ok_action(move |_, app| {
                        ok_runs.set(ok_runs.get() + 1);
                        dialog.close(app);
                    })
            })
            .fade(false)
            .on_close(record_close(reason.clone())),
            "alert",
        );
        assert!(
            dialog
                .with_alert(|alert| alert.ok_button().node())
                .flatten()
                .is_some(),
            "alert 面板必须挂进对话框内容列"
        );
        let ok = dialog
            .with_alert(|alert| alert.ok_button().node())
            .flatten()
            .expect("alert OK 节点");
        harness.click_node(ok);
        assert_eq!(ok_runs.get(), 1, "alert 的 OK 动作必须运行");
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        assert!(
            dialog.with_alert(|_| ()).is_some(),
            "关闭后 alert 句柄保留到 release（应用层信号生命周期）"
        );
    }

    #[test]
    fn an_alert_that_rejects_keeps_the_dialog_open_and_runs_nothing() {
        let mut harness = Harness::new();
        let ok_runs = Rc::new(Cell::new(0u32));
        let ok_runs_for_builder = ok_runs.clone();
        let dialog = harness.dialog(
            DialogBuilder::alert("Rename", move |dialog, _theme| {
                let dialog = dialog.clone();
                let ok_runs = ok_runs_for_builder.clone();
                AlertBuilder::new()
                    .ok("Rename", "Enter a name.")
                    .validate(|value| {
                        if value.is_empty() {
                            Err(smol_str::SmolStr::new_static("a name is required"))
                        } else {
                            Ok(())
                        }
                    })
                    .ok_action(move |_, app| {
                        ok_runs.set(ok_runs.get() + 1);
                        dialog.close(app);
                    })
            })
            .fade(false),
            "alert-validation",
        );
        let ok = dialog
            .with_alert(|alert| alert.ok_button().node())
            .flatten()
            .expect("alert OK 节点");
        harness.click_node(ok);
        assert_eq!(ok_runs.get(), 0, "校验失败不得调用 OK 动作");
        assert!(
            dialog.is_open(harness.app.app()),
            "alert 没有请求关闭 ⇒ 对话框保持打开"
        );
        assert!(
            dialog
                .with_alert(|alert| alert.has_error(harness.app.app()))
                .unwrap_or(false),
            "拒绝原因留在 alert 自己的错误状态里"
        );
        dialog.close_now(harness.app.app_mut());
        assert!(!dialog.is_open(harness.app.app()));
    }

    #[test]
    fn a_settled_dialog_requests_no_frames() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Idle")
                .fade(false)
                .content(content_box(40.0, 20.0)),
            "idle",
        );
        assert!(
            !dialog.sync(harness.app.app_mut()).expect("同步"),
            "稳态 sync 返回 false（零写入）"
        );
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "稳态不请求帧（I3）");
    }

    #[test]
    fn opening_an_open_dialog_is_idempotent() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(DialogBuilder::new("Idempotent").fade(false), "idempotent");
        let first = dialog.popup_node();
        assert!(
            !dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("重复打开"),
            "已打开时 open 返回 false"
        );
        harness.settle(&dialog);
        assert_eq!(dialog.popup_node(), first, "不得重建浮层内容");
        assert_eq!(harness.modal_entries().len(), 1, "不得叠出第二个对话框");
    }

    #[test]
    fn closing_twice_is_idempotent_and_notifies_once() {
        let mut harness = Harness::new();
        let closes = Rc::new(Cell::new(0u32));
        let dialog = harness.dialog(
            DialogBuilder::new("Twice").on_close({
                let closes = closes.clone();
                move |_reason, _app| closes.set(closes.get() + 1)
            }),
            "twice",
        );
        harness.tick(&dialog, Duration::from_millis(200));
        assert!(dialog.close(harness.app.app_mut()));
        assert!(
            dialog.close(harness.app.app_mut()),
            "淡出中再次关闭幂等返回 true"
        );
        assert!(dialog.is_closing(harness.app.app()));
        assert!(harness.settle_closed(&dialog));
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(closes.get(), 1, "关闭回调每完成一次关闭只派发一次");
    }

    #[test]
    fn reopening_rebuilds_the_panel_and_keeps_the_callbacks() {
        let mut harness = Harness::new();
        let clicks = Rc::new(Cell::new(0u32));
        let dialog = harness.dialog(
            DialogBuilder::new("Reopen").fade(false).confirm("OK", {
                let clicks = clicks.clone();
                move |_| clicks.set(clicks.get() + 1)
            }),
            "reopen",
        );
        let first = dialog.panel_node().expect("首个面板");
        dialog.close_now(harness.app.app_mut());
        assert!(!harness.tree_has(first));
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("重开")
        );
        harness.settle(&dialog);
        let second = dialog.panel_node().expect("新面板");
        assert_ne!(first, second, "重开必须重建面板");
        assert!(harness.tree_has(second));
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert_eq!(clicks.get(), 1, "重开后回调仍然可用");
    }

    #[test]
    fn a_second_dialog_stacks_above_the_first_and_only_the_top_one_closes() {
        let mut harness = Harness::new();
        let first_clicks = Rc::new(Cell::new(0u32));
        let first_reason = Rc::new(Cell::new(None));
        let first = harness.build(
            DialogBuilder::new("First")
                .fade(false)
                .confirm("OK", {
                    let clicks = first_clicks.clone();
                    move |_| clicks.set(clicks.get() + 1)
                })
                .on_close(record_close(first_reason.clone())),
            "first",
        );
        assert!(
            first
                .open(harness.app.app_mut(), harness.window)
                .expect("打开第一层")
        );
        harness.settle(&first);
        let second_clicks = Rc::new(Cell::new(0u32));
        let second_reason = Rc::new(Cell::new(None));
        let second = harness.build(
            DialogBuilder::new("Second")
                .fade(false)
                .confirm("OK", {
                    let clicks = second_clicks.clone();
                    move |_| clicks.set(clicks.get() + 1)
                })
                .on_close(record_close(second_reason.clone())),
            "second",
        );
        assert!(
            second
                .open(harness.app.app_mut(), harness.window)
                .expect("打开第二层")
        );
        harness.settle(&second);
        assert_eq!(harness.modal_entries().len(), 2, "模态可叠多层");
        let second_entry = harness.entry_of(&second);
        let first_entry = harness.entry_of(&first);
        assert!(second_entry.z > first_entry.z, "后开的模态 z 更大");
        assert_eq!(
            harness.modal_entries().last().map(|entry| entry.node),
            Some(second_entry.node),
            "最上层模态 = 第二层"
        );
        let ok = second.confirm_button().expect("确认").node().expect("节点");
        harness.click_node(ok);
        let _ = second.sync(harness.app.app_mut()).expect("收尾");
        assert_eq!(second_clicks.get(), 1, "只跑顶层内容自己的回调");
        assert_eq!(first_clicks.get(), 0, "下层内容不得被穿越点击");
        assert!(!second.is_open(harness.app.app()));
        assert_eq!(second_reason.get(), Some(DialogClose::Confirmed));
        assert!(first.is_open(harness.app.app()), "下层保持打开");
        assert_eq!(first_reason.get(), None);
        let first_ok = first.confirm_button().expect("确认").node().expect("节点");
        harness.click_node(first_ok);
        let _ = first.sync(harness.app.app_mut()).expect("收尾");
        assert_eq!(first_clicks.get(), 1);
        assert!(!first.is_open(harness.app.app()));
        assert_eq!(first_reason.get(), Some(DialogClose::Confirmed));
    }

    #[test]
    fn opening_a_dialog_dismisses_an_open_context_menu() {
        let mut harness = Harness::new();
        let context_menu = ContextMenu::new(
            harness.app.app_mut(),
            harness.theme.clone(),
            "context",
            [MenuEntry::Action(MenuAction::new("copy", "Copy"))],
            ContextMenuOptions::default(),
        );
        assert!(
            context_menu
                .open(
                    harness.app.app_mut(),
                    harness.window,
                    LogicalPoint::new(px(40.0), px(40.0))
                )
                .expect("打开右键菜单")
        );
        let menu_node = context_menu.popup_node().expect("菜单节点");
        let _ = harness.app.drain().expect("帧");
        assert_eq!(harness.stack().entries.len(), 1);

        let dialog = harness.dialog(DialogBuilder::new("Dialog").fade(false), "take-menu");
        assert!(
            !harness.tree_has(menu_node),
            "打开模态对话框必须先收起 Auto 根链（context menu）"
        );
        assert_eq!(harness.modal_entries().len(), 1, "模态接管后只剩对话框");
        assert!(dialog.is_open(harness.app.app()));
    }

    #[test]
    fn a_second_alert_stacks_above_the_first() {
        let mut harness = Harness::new();
        let first_reason = Rc::new(Cell::new(None));
        let first = harness.build(
            DialogBuilder::alert("First alert", |_dialog, _theme| {
                AlertBuilder::new().ok("First alert", "One at a time.")
            })
            .fade(false)
            .on_close(record_close(first_reason.clone())),
            "alert-a",
        );
        assert!(
            first
                .open(harness.app.app_mut(), harness.window)
                .expect("打开第一个 alert")
        );
        harness.settle(&first);
        let first_node = first.popup_node().expect("第一个 alert 节点");

        let second = harness.build(
            DialogBuilder::alert("Second alert", |_dialog, _theme| {
                AlertBuilder::new().ok("Second alert", "Stacks above the first.")
            })
            .fade(false),
            "alert-b",
        );
        assert!(
            second
                .open(harness.app.app_mut(), harness.window)
                .expect("打开第二个 alert")
        );
        harness.settle(&second);
        assert!(
            harness.tree_has(first_node),
            "P1 起 Alert 不再独占槽：后开的叠加，不替换先开的（旧独占槽语义已删除）"
        );
        assert_eq!(harness.modal_entries().len(), 2);
        assert!(first.is_open(harness.app.app()));
        assert_eq!(first_reason.get(), None);
    }

    #[test]
    fn sync_on_a_closed_dialog_is_a_no_op() {
        let mut harness = Harness::new();
        let dialog = harness.build(DialogBuilder::new("Closed").fade(false), "closed");
        assert!(
            !dialog.sync(harness.app.app_mut()).expect("同步"),
            "未打开的对话框不做事"
        );
        assert!(dialog.popup_node().is_none());
        dialog.release(harness.app.app_mut());
    }

    #[test]
    fn the_close_button_is_hittable_when_the_header_is_hidden() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Headerless")
                .header(false)
                .fade(false)
                .content(content_box(320.0, 40.0))
                .confirm("OK", |_| panic!("关闭按钮不得跑确认回调"))
                .cancel("Cancel", |_| panic!("关闭按钮不得跑取消回调"))
                .on_close(record_close(reason.clone())),
            "headerless-close",
        );
        let close = dialog
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("关闭按钮节点");
        harness.click_node(close);
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(
            !dialog.is_open(harness.app.app()),
            "无标题行时内容列从面板顶端开始：关闭按钮必须最后建才不被盖住（D1）"
        );
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn the_close_button_declares_an_a11y_name() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(DialogBuilder::new("Named").fade(false), "close-a11y");
        let close = dialog
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("关闭按钮节点");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(close).expect("关闭按钮必须在 a11y 树里");
        assert_eq!(record.role, A11yRole::Button);
        assert_eq!(record.label_text(), Some("Close"), "默认可访问名");

        let localized = harness.build(
            DialogBuilder::new("Named custom")
                .fade(false)
                .close_label("关闭"),
            "close-a11y-custom",
        );
        assert!(
            localized
                .open(harness.app.app_mut(), harness.window)
                .expect("打开")
        );
        harness.settle(&localized);
        let localized_close = localized
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("关闭按钮节点");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot
            .node_for(localized_close)
            .expect("本地化关闭按钮在 a11y 树里");
        assert_eq!(record.label_text(), Some("关闭"), "调用方可以给本地化名字");
    }

    #[test]
    fn an_alert_dialog_never_builds_a_close_button() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::alert("No close", |_dialog, _theme| {
                AlertBuilder::new().ok_cancel("No close", "Old alerts have no close button.")
            })
            .close_button(true)
            .fade(false),
            "alert-no-close",
        );
        assert!(
            dialog.close_button().is_none(),
            "alert 形状恒不建关闭按钮（旧 alert 语义；X 无法路由到 cancel_action）"
        );
    }

    #[test]
    fn escape_closes_an_alert_without_running_its_actions() {
        let mut harness = Harness::new();
        let ok_runs = Rc::new(Cell::new(0u32));
        let cancel_runs = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::alert("Esc alert", {
                let ok_runs = ok_runs.clone();
                let cancel_runs = cancel_runs.clone();
                move |_dialog, _theme| {
                    let ok_counter = ok_runs.clone();
                    let cancel_counter = cancel_runs.clone();
                    AlertBuilder::new()
                        .ok_cancel("Esc alert", "Esc closes it.")
                        .ok_action(move |_, _| ok_counter.set(ok_counter.get() + 1))
                        .cancel_action(move |_| cancel_counter.set(cancel_counter.get() + 1))
                }
            })
            .on_close(record_close(reason.clone())),
            "alert-esc",
        );
        harness.escape();
        assert!(harness.settle_closed(&dialog));
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Dismissed),
            "Esc = Dismissed"
        );
        assert_eq!(ok_runs.get(), 0);
        assert_eq!(
            cancel_runs.get(),
            0,
            "Esc 不跑 alert 的 cancel_action（无应用层关闭钩子）"
        );
    }

    #[test]
    fn a_second_click_during_the_fade_runs_no_action() {
        let mut harness = Harness::new();
        let confirms = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Double click")
                .confirm("OK", {
                    let confirms = confirms.clone();
                    move |_| confirms.set(confirms.get() + 1)
                })
                .cancel("Cancel", |_| panic!("取消回调不得被双击触发"))
                .on_close(record_close(reason.clone())),
            "double-click",
        );
        harness.tick(&dialog, Duration::from_millis(200));
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert_eq!(confirms.get(), 1, "第一次点击跑确认回调");
        assert!(dialog.is_closing(harness.app.app()), "第一次点击开始淡出");
        harness.click_node(ok);
        assert_eq!(
            confirms.get(),
            1,
            "淡出中的第二次点击不得再跑用户回调（D2）"
        );
        assert!(harness.settle_closed(&dialog));
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Confirmed),
            "关闭原因保持第一次的 Confirmed"
        );
    }

    #[test]
    fn reopening_while_closing_finishes_the_close_then_reopens() {
        let mut harness = Harness::new();
        let closes = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Reopen while closing").on_close({
                let closes = closes.clone();
                let reason = reason.clone();
                move |closed, _app| {
                    closes.set(closes.get() + 1);
                    reason.set(Some(closed));
                }
            }),
            "reopen-closing",
        );
        harness.tick(&dialog, Duration::from_millis(200));
        assert!(dialog.close(harness.app.app_mut()));
        assert!(dialog.is_closing(harness.app.app()), "进入淡出");
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("关闭中重开"),
            "关闭中 open 必须真的重开（不得返回 Ok(false) 静默丢弃）"
        );
        assert!(dialog.is_open(harness.app.app()));
        assert!(!dialog.is_closing(harness.app.app()));
        assert_eq!(closes.get(), 1, "上一次关闭必须先收尾（on_close 派发一次）");
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        harness.settle(&dialog);
        let panel = dialog.panel_node().expect("重开后的面板");
        assert!(harness.tree_has(panel), "重开必须有新面板");
        assert_eq!(harness.modal_entries().len(), 1, "旧浮层不得残留");
    }

    #[test]
    fn reopening_before_the_closed_state_is_synced_reopens() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Unsynced")
                .fade(false)
                .on_close(record_close(reason.clone())),
            "reopen-unsynced",
        );
        let close = dialog
            .close_button()
            .expect("关闭按钮")
            .node()
            .expect("关闭按钮节点");
        harness.click_node(close);
        assert!(
            dialog.is_open(harness.app.app()),
            "fade=false 已卸载：kit 关闭状态在帧边界收尾"
        );
        assert!(
            dialog.state(harness.app.app()).is_none(),
            "引擎侧已经移除栈项"
        );
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("未收尾时重开"),
            "未收尾的关闭必须先收尾再重开"
        );
        assert!(dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        harness.settle(&dialog);
        assert!(harness.tree_has(dialog.panel_node().expect("新面板")));
    }

    #[test]
    fn close_now_keeps_the_reason_the_close_started_with() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Reason")
                .confirm("OK", |_| {})
                .on_close(record_close(reason.clone())),
            "reason",
        );
        harness.tick(&dialog, Duration::from_millis(200));
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert!(dialog.is_closing(harness.app.app()));
        assert!(dialog.close_now(harness.app.app_mut()), "立即关闭");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Confirmed),
            "close_now 不得把 Confirmed 改写成 Dismissed（D8）"
        );
    }

    #[test]
    fn release_removes_the_dialog_button_action_handlers() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Release")
                .fade(false)
                .confirm("OK", |_| {})
                .cancel("Cancel", |_| {}),
            "release",
        );
        let actions: Vec<Action> = [
            dialog.confirm_button(),
            dialog.cancel_button(),
            dialog.close_button(),
        ]
        .into_iter()
        .flatten()
        .map(|button| button.action_identity().expect("按钮动作身份"))
        .collect();
        assert_eq!(actions.len(), 3, "确认 / 取消 / 关闭三个按钮");
        for action in &actions {
            assert!(
                harness.app.app().has_action_handler(action),
                "{action:?} 必须在册"
            );
        }
        dialog.release(harness.app.app_mut());
        for action in &actions {
            assert!(
                !harness.app.app().has_action_handler(action),
                "{action:?} 必须在 release 后摘除（D9）"
            );
        }
        dialog.release(harness.app.app_mut());
        assert!(dialog.close_button().is_none(), "release 后按钮句柄已释放");
    }

    #[test]
    fn release_removes_the_alert_content_action_handlers() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::alert("Release alert", |_dialog, _theme| {
                AlertBuilder::new()
                    .ok_cancel("Release alert", "Content")
                    .ok_action(|_, _| {})
                    .cancel_action(|_| {})
            })
            .fade(false),
            "release-alert",
        );
        let ok_action = dialog
            .with_alert(|alert| alert.ok_button().action_identity())
            .flatten()
            .expect("alert OK 动作身份");
        let cancel_action = dialog
            .with_alert(|alert| {
                alert
                    .cancel_button()
                    .and_then(|button| button.action_identity())
            })
            .flatten()
            .expect("alert Cancel 动作身份");
        for action in [&ok_action, &cancel_action] {
            assert!(harness.app.app().has_action_handler(action));
        }
        dialog.release(harness.app.app_mut());
        for action in [&ok_action, &cancel_action] {
            assert!(
                !harness.app.app().has_action_handler(action),
                "{action:?} 必须在 release 后摘除（alert 内容侧）"
            );
        }
    }

    #[test]
    fn a_settled_dialog_with_the_default_fade_requests_no_frames() {
        let mut harness = Harness::new();
        let dialog = harness.dialog(
            DialogBuilder::new("Idle fade").content(content_box(40.0, 20.0)),
            "idle-fade",
        );
        harness.tick(&dialog, Duration::from_millis(200));
        assert!(
            !dialog.sync(harness.app.app_mut()).expect("同步"),
            "淡入完成后的稳态 sync 必须返回 false（I3，默认 fade=true）"
        );
        let outcome = harness
            .app
            .app_mut()
            .frame(harness.window)
            .expect("窗口")
            .expect("帧");
        assert_eq!(outcome, FrameOutcome::Idle, "稳态不请求帧");
    }

    #[test]
    fn an_alert_shape_declares_exactly_one_dialog_role() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(
            DialogBuilder::alert("Delete file", |_dialog, _theme| {
                AlertBuilder::new().ok_cancel("Delete file", "This cannot be undone.")
            })
            .fade(false),
            "alert-role-unique",
        );
        let wrapper = dialog.popup_node().expect("浮层内容节点");
        let content = dialog
            .with_alert(|alert| alert.root_node())
            .expect("alert 内容根");
        let snapshot = full_a11y(&mut harness.app);
        let dialog_roles = snapshot
            .nodes()
            .iter()
            .filter(|node| matches!(node.role, A11yRole::Dialog | A11yRole::AlertDialog))
            .count();
        assert_eq!(
            dialog_roles, 1,
            "alert 形状全树只能有一个对话框角色（P2-4d1 审查 D6）"
        );
        assert_eq!(a11y_of(&snapshot, wrapper).role, A11yRole::AlertDialog);
        let record = a11y_of(&snapshot, content);
        assert_eq!(
            record.role,
            A11yRole::GenericContainer,
            "alert 内容根不得重复声明对话框角色"
        );
        assert_eq!(
            record.label_text(),
            None,
            "可访问名归包装层，内容根不重复标题"
        );
    }

    #[test]
    fn a_second_click_on_alert_content_during_the_fade_runs_no_action() {
        let mut harness = Harness::new();
        let ok_runs = Rc::new(Cell::new(0u32));
        let cancel_runs = Rc::new(Cell::new(0u32));
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::alert("Double click", {
                let ok_runs = ok_runs.clone();
                let cancel_runs = cancel_runs.clone();
                move |dialog, _theme| {
                    let dialog = dialog.clone();
                    let ok = ok_runs.clone();
                    let cancel = cancel_runs.clone();
                    AlertBuilder::new()
                        .ok_cancel("Double click", "Delete?")
                        .ok_action(move |_, app| {
                            ok.set(ok.get() + 1);
                            dialog.close(app);
                        })
                        .cancel_action(move |_| cancel.set(cancel.get() + 1))
                }
            })
            .on_close(record_close(reason.clone())),
            "alert-double-click",
        );
        harness.tick(&dialog, Duration::from_millis(200));
        let ok = dialog
            .with_alert(|alert| alert.ok_button().node())
            .flatten()
            .expect("alert OK 节点");
        let cancel = dialog
            .with_alert(|alert| alert.cancel_button().and_then(|button| button.node()))
            .flatten()
            .expect("alert Cancel 节点");
        harness.click_node(ok);
        assert_eq!(ok_runs.get(), 1, "第一次点击跑 alert 的 OK 动作");
        assert!(
            dialog.is_closing(harness.app.app()),
            "OK 动作里的 close 开始淡出"
        );
        harness.click_node(ok);
        assert_eq!(
            ok_runs.get(),
            1,
            "淡出中的第二次点击不得再跑 alert 的 OK 动作（D2）"
        );
        harness.click_node(cancel);
        assert_eq!(
            cancel_runs.get(),
            0,
            "淡出中 alert 的 Cancel 动作同样不得跑"
        );
        assert!(harness.settle_closed(&dialog));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        assert_eq!(ok_runs.get(), 1, "整段淡出只跑一次 OK 动作");
    }

    #[test]
    fn a_modal_dialog_repaints_fully_when_the_scrim_changes() {
        let mut harness = Harness::new();
        harness.drain_until_idle();
        let dialog = harness.build(DialogBuilder::new("Scrim damage").fade(false), "damage");
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("打开")
        );
        let opening = harness.app.drain().expect("打开帧").clone();
        assert!(
            opening.damage_full,
            "模态打开要绘制全视口遮罩：允许整帧（设计 I6 模态例外）"
        );
        for _ in 0..4 {
            let record = harness.app.drain().expect("帧").clone();
            if record.rendered {
                assert!(!record.damage_full, "模态稳定后不得再整帧重绘");
            }
        }
        assert!(dialog.close(harness.app.app_mut()));
        let mut removal_full = false;
        for _ in 0..40 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let record = harness.app.drain().expect("帧").clone();
            removal_full |= record.damage_full;
            if !dialog.is_open(harness.app.app()) {
                break;
            }
        }
        assert!(removal_full, "模态关闭回收全视口遮罩必须整帧");
        assert!(harness.scrim_node().is_none());
    }

    #[test]
    fn clicking_an_input_inside_the_dialog_focuses_it() {
        let mut harness = Harness::new();
        let state = harness.app.app_mut().signal(InputState::new(""));
        let placeholder = harness.app.app_mut().signal(SmolStr::new_static(""));
        let spec = Arc::new(InputSpec {
            name: SmolStr::new_static("dialog-input"),
            label: None,
            placeholder: SmolStr::new_static(""),
            masked: false,
            multiline: false,
            readonly: false,
            disabled: false,
            max_length: None,
            visible_lines: None,
            size: ControlSize::Medium,
            submit: None,
        });
        let theme = harness.theme.clone();
        let dialog = harness.dialog(
            DialogBuilder::new("Dialog input").fade(false).content(
                move |app: &mut App, window: WindowId, parent: NodeId| {
                    let tree = app
                        .window_mut(window)
                        .ok_or(ui_core::error::EngineError::UnknownWindow { window })?
                        .tree_mut();
                    let _ =
                        kit_input::build(tree, parent, spec.clone(), &theme, &state, &placeholder);
                    Ok(())
                },
            ),
            "dialog-input",
        );
        let input_root = harness.find("dialog-input.root").expect("输入框根");
        let input_text = harness.find("dialog-input.text").expect("输入框文本节点");
        assert_ne!(
            harness.focused(),
            Some(input_text),
            "初始焦点是默认按钮，不是输入框"
        );
        harness.click_node(input_root);
        assert_eq!(
            harness.focused(),
            Some(input_text),
            "点击模态对话框内的输入框必须取焦并保持模态作用域"
        );
        assert!(dialog.is_open(harness.app.app()));
    }
}
