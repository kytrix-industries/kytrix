use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::{Rc, Weak},
};

use smol_str::{SmolStr, format_smolstr};
use taffy::style::Position;
use ui_core::{
    a11y::A11yPatch,
    app::App,
    error::EngineError,
    interaction::ActionCtx,
    math::{LogicalPoint, LogicalPx, px},
    node::{
        builder::NodeBuilder,
        id::NodeId,
        signal::{Signal, SignalScope},
        tree::NodeTree,
    },
    paint::{style::StylePatch, style_builder::StyleBuilder},
    text::{TextAlign, TextStyle},
    window::WindowId,
};

use crate::{
    kit::{
        alert::{Alert, AlertBuilder},
        button::{Button, stretch_button},
        icon::IconKind,
        layer::Band,
        overlay::{
            Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayA11y, OverlayError,
            OverlayOptions, OverlaySurface, Placement, Scrim,
        },
        size::UnifiedSizable,
    },
    theme::{handle::ThemeHandle, role::Role},
};

const CONTENT_WIDTH: f32 = 384.0;

const CONTENT_GAP: f32 = 8.0;

const TITLE_SIZE: f32 = 20.0;

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
    Overlay(OverlayError),
    Engine(EngineError),
    NotOpen,
}

impl From<OverlayError> for DialogError {
    fn from(error: OverlayError) -> Self {
        Self::Overlay(error)
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
            Self::Overlay(error) => error.fmt(formatter),
            Self::Engine(error) => error.fmt(formatter),
            Self::NotOpen => formatter.write_str("dialog overlay is not open"),
        }
    }
}

impl std::error::Error for DialogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Overlay(error) => Some(error),
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
    dismiss: DismissPolicy,
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
            dismiss: DismissPolicy::Explicit,
            fade: true,
        }
    }

    fn band(&self) -> Band {
        match self.role {
            DialogRole::Dialog => Band::Dialog,
            DialogRole::AlertDialog => Band::Alert,
        }
    }

    fn a11y(&self, title: &str) -> OverlayA11y {
        match self.role {
            DialogRole::Dialog => OverlayA11y::dialog(title),
            DialogRole::AlertDialog => OverlayA11y::alert_dialog(title),
        }
    }
}

#[derive(Default)]
struct DialogState {
    window: Option<WindowId>,
    body: Option<NodeId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseStart {
    Started,
    AlreadyClosing,
    NotOpen,
}

struct DialogInner {
    overlay: Overlay,
    config: DialogConfig,
    theme: ThemeHandle,
    scope: SignalScope,
    open: Signal<bool>,
    title: Signal<SmolStr>,
    closing: Cell<bool>,
    reason: Cell<Option<DialogClose>>,
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
    pub fn dismiss(mut self, dismiss: DismissPolicy) -> Self {
        self.config.dismiss = dismiss;
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
    pub fn build(
        self,
        app: &mut App,
        theme: ThemeHandle,
        layer: crate::kit::layer::Layer,
        router: AnimationRouter,
        key: impl Into<SmolStr>,
    ) -> Dialog {
        let key = key.into();
        let scope = app.signal_scope();
        let open = app.signal_scoped(scope, false);
        let title = app.signal_scoped(scope, self.config.title.clone());
        let overlay = Overlay::new(
            app,
            theme.clone(),
            layer,
            router,
            key,
            OverlayOptions {
                placement: Placement::TOP_START,
                gap: LogicalPx::ZERO,
                margin: LogicalPx::ZERO,
                flip: false,
                dismiss: self.config.dismiss,
                modal: true,
                restore_focus: true,
                band: self.config.band(),
                slot: crate::kit::layer::LayerSlot::Exclusive,
                surface: OverlaySurface::dialog(),
                scrim: Some(Scrim::default()),
                a11y: self.config.a11y(self.config.title.as_str()),
                fade: self.config.fade,
            },
        );
        Dialog {
            inner: Rc::new(DialogInner {
                overlay,
                config: self.config,
                theme,
                scope,
                open,
                title,
                closing: Cell::new(false),
                reason: Cell::new(None),
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
            }),
        }
    }
}

impl Dialog {
    pub fn open(&self, app: &mut App, window: WindowId) -> Result<bool, DialogError> {
        if *app.read(&self.inner.open) {
            if !self.inner.closing.get() && self.inner.overlay.is_open(app) {
                return Ok(false);
            }
            self.inner.overlay.close_now(app);
            self.finalize(app);
        }
        if self.inner.overlay.is_open(app) {
            self.inner.overlay.close_now(app);
        }
        self.release_content(app);
        let viewport = app
            .window(window)
            .ok_or(EngineError::UnknownWindow { window })?
            .size();
        let title = app.read(&self.inner.title).clone();
        self.inner
            .overlay
            .set_a11y(self.inner.config.a11y(title.as_str()));
        let anchor = Anchor::point(LogicalPoint::new(
            viewport.width * 0.5,
            viewport.height * 0.5,
        ));
        let opened = self.inner.overlay.open(
            app,
            window,
            anchor,
            |_tree: &mut NodeTree, _panel: NodeId| -> Result<(), EngineError> { Ok(()) },
        )?;
        if !opened {
            return Ok(false);
        }
        self.inner.state.borrow_mut().window = Some(window);
        let built = self
            .build_content(app, window)
            .and_then(|()| self.build_alert(app, window));
        if let Err(error) = built {
            self.inner.overlay.close_now(app);
            self.release_content(app);
            self.inner.state.borrow_mut().window = None;
            return Err(error);
        }
        self.inner.reason.set(None);
        self.inner.closing.set(false);
        let _ = app.write(&self.inner.open, true);
        Ok(true)
    }

    pub fn close(&self, app: &mut App) -> bool {
        !matches!(
            self.begin_close(app, DialogClose::Dismissed),
            CloseStart::NotOpen
        )
    }

    pub fn close_now(&self, app: &mut App) -> bool {
        if !*app.read(&self.inner.open) {
            return false;
        }
        self.inner.closing.set(true);
        if self.inner.reason.get().is_none() {
            self.inner.reason.set(Some(DialogClose::Dismissed));
        }
        self.inner.overlay.close_now(app);
        self.finalize(app);
        true
    }

    pub fn sync(&self, app: &mut App) -> Result<bool, DialogError> {
        if !*app.read(&self.inner.open) {
            return Ok(false);
        }
        let mut changed = self.recenter(app);
        changed |= self.inner.overlay.sync(app);
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
        if !self.inner.overlay.is_open(app) {
            self.finalize(app);
            changed = true;
        }
        Ok(changed)
    }

    #[must_use]
    pub fn is_open(&self, app: &App) -> bool {
        *app.read(&self.inner.open)
    }

    #[must_use]
    pub fn is_closing(&self) -> bool {
        self.inner.closing.get()
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
        self.inner
            .overlay
            .set_a11y(self.inner.config.a11y(title.as_str()));
        let window = self.inner.state.borrow().window;
        if let (Some(window), Some(wrapper)) = (window, self.inner.overlay.overlay_node()) {
            let _ = app.set_a11y_patch(
                window,
                wrapper,
                A11yPatch {
                    label: Some(title),
                    ..A11yPatch::default()
                },
            );
        }
        true
    }

    #[must_use]
    pub fn key(&self) -> &str {
        self.inner.overlay.key()
    }

    #[must_use]
    pub fn open_state(&self) -> Signal<bool> {
        self.inner.overlay.open_state()
    }

    #[must_use]
    pub fn overlay_node(&self) -> Option<NodeId> {
        self.inner.overlay.overlay_node()
    }

    #[must_use]
    pub fn panel_node(&self) -> Option<NodeId> {
        self.inner.overlay.panel_node()
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
        self.inner.overlay.release(app);
        self.release_content(app);
        self.inner.closing.set(false);
        let _ = self.inner.reason.take();
        let _ = app.write(&self.inner.open, false);
        self.inner.state.borrow_mut().window = None;
        let _ = app.release_scope(self.inner.scope);
    }

    fn build_content(&self, app: &mut App, window: WindowId) -> Result<(), DialogError> {
        let panel = self
            .inner
            .overlay
            .panel_node()
            .ok_or(DialogError::NotOpen)?;
        let key = self.inner.overlay.key();
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
            .build(tree_mut(app, window)?, panel)?;
        if self.inner.config.header && !self.inner.config.title.is_empty() {
            let title_theme = self.inner.theme.clone();
            let title = self.inner.title;
            NodeBuilder::text(self.inner.config.title.clone(), {
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
        }
        let body = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:body"))
            .style(
                StyleBuilder::new()
                    .flex_col()
                    .width_full()
                    .gap(px(CONTENT_GAP))
                    .build(),
            )
            .build(tree_mut(app, window)?, root)?;
        self.inner.state.borrow_mut().body = Some(body);
        let content = self.inner.content.borrow().clone();
        if let Some(content) = content {
            content(app, window, body)?;
        }
        if self.inner.confirm.is_some() || self.inner.cancel.is_some() {
            self.build_button_row(app, window, root, key)?;
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
    ) -> Result<(), DialogError> {
        let row = NodeBuilder::boxed()
            .id(format_smolstr!("{key}:buttons"))
            .style(StyleBuilder::new().flex_row().width_full().build())
            .build(tree_mut(app, window)?, root)?;
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

    fn build_alert(&self, app: &mut App, window: WindowId) -> Result<(), DialogError> {
        let source = self.inner.alert_source.borrow().clone();
        let Some(source) = source else {
            return Ok(());
        };
        let body = self
            .inner
            .state
            .borrow()
            .body
            .ok_or(DialogError::Engine(EngineError::UnknownWindow { window }))?;
        let builder = source(self, &self.inner.theme);
        let weak = Rc::downgrade(&self.inner);
        let builder =
            builder.closing_guard(move || weak.upgrade().is_some_and(|inner| inner.closing.get()));
        let alert = builder.build(app, window, body, &self.inner.theme)?;
        *self.inner.alert.borrow_mut() = Some(alert);
        Ok(())
    }

    fn recenter(&self, app: &mut App) -> bool {
        if self.inner.closing.get() {
            return false;
        }
        let (Some(window), Some(node)) = (
            self.inner.state.borrow().window,
            self.inner.overlay.overlay_node(),
        ) else {
            return false;
        };
        let Some(viewport) = app.window(window).map(|target| target.size()) else {
            return false;
        };
        let Some(bounds) = app
            .window(window)
            .and_then(|target| target.tree().get(node).ok())
            .map(|target| target.layout().bounds)
        else {
            return false;
        };
        let desired = LogicalPoint::new(
            ((viewport.width - bounds.size.width) * 0.5).max(px(0.0)),
            ((viewport.height - bounds.size.height) * 0.5).max(px(0.0)),
        );
        if (bounds.origin.x - desired.x).abs().value() < 0.5
            && (bounds.origin.y - desired.y).abs().value() < 0.5
        {
            return false;
        }
        let anchor = Anchor::point(LogicalPoint::new(desired.x, desired.y + bounds.size.height));
        self.inner
            .overlay
            .open(
                app,
                window,
                anchor,
                |_tree: &mut NodeTree, _panel: NodeId| -> Result<(), EngineError> { Ok(()) },
            )
            .is_ok()
    }

    fn begin_close(&self, app: &mut App, reason: DialogClose) -> CloseStart {
        if !*app.read(&self.inner.open) {
            return CloseStart::NotOpen;
        }
        if self.inner.closing.get() {
            return CloseStart::AlreadyClosing;
        }
        self.inner.closing.set(true);
        self.inner.reason.set(Some(reason));
        self.inner.overlay.close(app);
        CloseStart::Started
    }

    fn finalize(&self, app: &mut App) {
        if !*app.read(&self.inner.open) {
            return;
        }
        let reason = self.inner.reason.take().unwrap_or(DialogClose::Dismissed);
        self.inner.closing.set(false);
        let _ = app.write(&self.inner.open, false);
        self.inner.state.borrow_mut().window = None;
        let callback = self.inner.on_close.borrow().clone();
        if let Some(callback) = callback {
            callback(reason, app);
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
    }
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
    use std::{cell::Cell, rc::Rc, time::Duration};

    use smol_str::{SmolStr, format_smolstr};
    use ui_core::{
        a11y::A11yRole,
        app::App,
        event::{Action, InputEvent, KeyCode, Modifiers, PointerButton},
        math::{LogicalPoint, LogicalRect, LogicalSize, px},
        node::{builder::NodeBuilder, id::NodeId, signal::Signal},
        paint::{
            color::Color,
            primitive::{Primitive, QuadPaint},
            style_builder::{PaintBuilder, StyleBuilder},
        },
        test_support::app::TestApp,
        window::{FrameOutcome, WindowId},
    };

    use super::{Dialog, DialogBuilder, DialogClose};
    use crate::{
        kit::{
            alert::AlertBuilder,
            layer::{Band, Layer},
            overlay::{Anchor, AnimationRouter, DismissPolicy, Overlay, OverlayOptions, Placement},
            test_support::{a11y_of, full_a11y},
        },
        theme::{appearance::Appearance, handle::ThemeHandle},
    };

    const VIEWPORT: LogicalSize = LogicalSize::new(px(800.0), px(600.0));

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
            let app = TestApp::new();
            let window = app.window_id();
            let root = app.app().window(window).expect("窗口").tree().root();
            let mut theme_app = app;
            let theme = ThemeHandle::new(theme_app.app_mut(), Appearance::Light);
            Self {
                app: theme_app,
                window,
                root,
                theme,
                layer: Layer::new(),
                router: AnimationRouter::new(),
            }
        }

        fn build(&mut self, builder: DialogBuilder, key: &str) -> Dialog {
            builder.build(
                self.app.app_mut(),
                self.theme.clone(),
                self.layer.clone(),
                self.router.clone(),
                key,
            )
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
            for _ in 0..4 {
                let _ = dialog.sync(self.app.app_mut()).expect("同步");
                let _ = self.app.drain().expect("帧");
            }
            let _ = dialog.sync(self.app.app_mut()).expect("同步");
        }

        fn bounds(&self, node: NodeId) -> LogicalRect {
            self.app.probe().bounds(node).expect("布局盒")
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

        fn escape(&mut self) {
            let _ = self
                .app
                .inject(InputEvent::key_down(KeyCode::Escape, Modifiers::empty()));
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

        fn focused_debug_id(&self) -> Option<String> {
            self.app.app().focused(self.window).and_then(|node| {
                self.app
                    .app()
                    .window(self.window)
                    .expect("窗口")
                    .tree()
                    .debug_id(node)
                    .map(str::to_owned)
            })
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

    fn opaque_background_alpha(app: &App, window: WindowId) -> Option<f32> {
        app.window(window)?
            .scene()
            .iter()
            .find_map(|primitive| match primitive {
                Primitive::Quad(quad) if quad.bounds.size == VIEWPORT => match &quad.paint {
                    QuadPaint::Solid { fill, .. } if color_alpha(*fill) >= 0.999 => {
                        Some(color_alpha(*fill))
                    }
                    _ => None,
                },
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
        assert!(dialog.overlay_node().is_none(), "关闭后浮层节点卸载");
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
    fn escape_closes_the_dialog_and_reports_dismissed() {
        let mut harness = Harness::new();
        let reason = Rc::new(Cell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Escape").on_close(record_close(reason.clone())),
            "escape",
        );
        let node = dialog.overlay_node().expect("浮层节点");
        harness.escape();
        assert!(
            !harness.tree_has(node),
            "引擎默认绑定 Esc → Dismiss：节点立即卸载"
        );
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Dismissed),
            "Esc 是 Dismissed"
        );
        assert_eq!(
            harness.app.app().modal_scope(harness.window),
            None,
            "关闭后不再有模态作用域"
        );
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
            "Explicit 不得被外部点击关闭"
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
                .dismiss(DismissPolicy::OutsideClick)
                .on_close(record_close(reason.clone())),
            "outside",
        );
        harness.click_at(LogicalPoint::new(px(10.0), px(10.0)));
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(
            !dialog.is_open(harness.app.app()),
            "OutsideClick 点遮罩即关闭"
        );
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn the_dialog_traps_focus_and_restores_it_on_close() {
        let mut harness = Harness::new();
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
            DialogBuilder::new("Focus")
                .fade(false)
                .confirm("OK", |_| {})
                .content(content_box(80.0, 20.0)),
            "focus",
        );
        let _ = harness.app.app_mut().focus_next(harness.window);
        let focused = harness.focused_debug_id().expect("焦点必须在对话框内");
        assert!(
            focused.contains(":ok") || focused.contains(":cancel") || focused.contains(":close"),
            "焦点必须落在对话框的可聚焦节点上，实际 {focused}"
        );
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        dialog.close_now(harness.app.app_mut());
        assert_eq!(
            harness.app.app().focused(harness.window),
            Some(trigger),
            "关闭后焦点必须还给 opener"
        );
    }

    #[test]
    fn the_dialog_is_the_modal_scope_and_the_background_stays_inert() {
        let mut harness = Harness::new();
        let (_background, clicks) = harness.background_with_actions();
        let dialog = harness.dialog(DialogBuilder::new("Modal").fade(false), "modal");
        assert_eq!(
            harness.app.app().modal_scope(harness.window),
            dialog.overlay_node(),
            "最上层模态作用域 = 对话框浮层包装节点"
        );
        harness.click_at(LogicalPoint::new(px(20.0), px(20.0)));
        assert_eq!(*harness.app.app().read(&clicks), 0, "模态期间背景不可交互");
    }

    #[test]
    fn the_dialog_group_fades_together_with_its_content() {
        let mut harness = Harness::new();
        let (_background, _clicks) = harness.background_with_actions();
        let tracked = Rc::new(std::cell::RefCell::new(None));
        let dialog = harness.dialog(
            DialogBuilder::new("Fade")
                .content(content_box_tracked(120.0, 40.0, tracked.clone()))
                .close_button(false),
            "fade",
        );
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let panel = harness.bounds(dialog.panel_node().expect("面板"));
        let content_node = tracked.borrow().expect("内容盒节点");
        let body = harness.bounds(content_node);
        let full_panel = quad_alpha(harness.app.app(), harness.window, panel).expect("面板背景");
        let full_body = quad_alpha(harness.app.app(), harness.window, body).expect("内容盒");
        let full_title = first_glyph_alpha(harness.app.app(), harness.window).expect("标题字形");
        let full_background = opaque_background_alpha(harness.app.app(), harness.window)
            .expect("背景节点的不透明 quad");
        assert!((full_panel - 1.0).abs() < 0.01, "淡入完成后面板不透明");
        assert!((full_body - 1.0).abs() < 0.01, "淡入完成后内容不透明");

        assert!(dialog.close(harness.app.app_mut()), "开始退场");
        harness.app.advance(Duration::from_millis(75));
        let _ = harness.app.drain().expect("退场帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let panel_progress =
            quad_alpha(harness.app.app(), harness.window, panel).expect("面板背景");
        let body_progress = quad_alpha(harness.app.app(), harness.window, body).expect("内容盒");
        let title_progress =
            first_glyph_alpha(harness.app.app(), harness.window).expect("标题字形");
        let background_progress =
            opaque_background_alpha(harness.app.app(), harness.window).expect("背景 quad");
        assert!(
            panel_progress < full_panel * 0.99,
            "退场中面板必须变淡：{panel_progress} / {full_panel}"
        );
        assert!(
            (body_progress / full_body - panel_progress / full_panel).abs() < 0.02,
            "E13 组不透明度：内容子节点必须跟面板同比例变淡（body {body_progress} / panel {panel_progress}）"
        );
        assert!(
            (title_progress / full_title - panel_progress / full_panel).abs() < 0.02,
            "文字也必须跟组淡出（title {title_progress} / panel {panel_progress}）"
        );
        assert!(
            (background_progress - full_background).abs() < 0.001,
            "对话框外的背景不参与淡出：{background_progress} / {full_background}"
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
        let node = dialog.overlay_node().expect("浮层节点");
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(dialog.close(harness.app.app_mut()), "开始退场");
        let mut unloaded = false;
        for _ in 0..24 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let _ = harness.app.drain().expect("帧");
            if !harness.tree_has(node) {
                unloaded = true;
                break;
            }
        }
        assert!(
            unloaded,
            "淡出到终值时 E10 完成回调必须在同一帧卸载浮层节点"
        );
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
    fn the_wrapper_declares_the_dialog_role_label_and_modal() {
        let mut harness = Harness::new();
        harness.app.enable_a11y_capture();
        let dialog = harness.dialog(DialogBuilder::new("Delete file").fade(false), "a11y");
        let wrapper = dialog.overlay_node().expect("包装节点");
        let panel = dialog.panel_node().expect("面板");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(wrapper).expect("包装节点 a11y");
        assert_eq!(record.role, A11yRole::Dialog, "显式 Dialog 角色");
        assert_eq!(record.label_text(), Some("Delete file"), "可访问名 = 标题");
        assert!(record.modal, "模态对话框必须声明 modal");
        assert_eq!(
            snapshot.node_for(panel).expect("面板 a11y").role,
            A11yRole::GenericContainer,
            "面板不得重复声明 Dialog"
        );
        let scrim = snapshot
            .nodes()
            .iter()
            .find(|record| record.role == A11yRole::Group)
            .expect("遮罩在 a11y 树里");
        assert!(scrim.hidden, "遮罩必须 hidden（纯装饰）");
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
        let wrapper = dialog.overlay_node().expect("包装节点");
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
            a11y_of(&snapshot, wrapper).label_text(),
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
            a11y_of(&snapshot, dialog.overlay_node().expect("包装节点")).label_text(),
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
        let wrapper = dialog.overlay_node().expect("包装节点");
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
        let wrapper = dialog.overlay_node().expect("包装节点");
        let snapshot = full_a11y(&mut harness.app);
        let record = snapshot.node_for(wrapper).expect("包装节点 a11y");
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
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(dialog.close(harness.app.app_mut()));
        assert!(
            dialog.close(harness.app.app_mut()),
            "淡出中再次关闭幂等返回 true"
        );
        for _ in 0..20 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let _ = harness.app.drain().expect("帧");
            if !dialog.is_open(harness.app.app()) {
                break;
            }
        }
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
        assert_eq!(harness.layer.len(harness.window), 2, "同带可叠多层");
        assert_eq!(
            harness.app.app().modal_scope(harness.window),
            second.overlay_node(),
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
        let menu = Overlay::new(
            harness.app.app_mut(),
            harness.theme.clone(),
            harness.layer.clone(),
            harness.router.clone(),
            "menu",
            OverlayOptions {
                placement: Placement::TOP_START,
                band: Band::ContextMenu,
                fade: false,
                ..OverlayOptions::default()
            },
        );
        assert!(
            menu.open(
                harness.app.app_mut(),
                harness.window,
                Anchor::point(LogicalPoint::new(px(40.0), px(40.0))),
                |_, _| Ok(())
            )
            .expect("打开菜单")
        );
        let menu_node = menu.overlay_node().expect("菜单节点");
        let _ = harness.app.drain().expect("帧");
        assert_eq!(harness.layer.len(harness.window), 1);

        let dialog = harness.dialog(DialogBuilder::new("Dialog").fade(false), "take-menu");
        assert!(
            !harness.tree_has(menu_node),
            "打开对话框必须先收起 context menu（旧 layer 语义）"
        );
        assert_eq!(harness.layer.len(harness.window), 1);
        assert!(dialog.is_open(harness.app.app()));
    }

    #[test]
    fn an_alert_dialog_replaces_the_previous_alert_slot() {
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
        let first_node = first.overlay_node().expect("第一个 alert 节点");

        let second = harness.build(
            DialogBuilder::alert("Second alert", |_dialog, _theme| {
                AlertBuilder::new().ok("Second alert", "Replaces the first.")
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
        let _ = first
            .sync(harness.app.app_mut())
            .expect("第一个 alert 收尾");
        assert!(
            !harness.tree_has(first_node),
            "alert 槽至多一个：后开的替换先开的"
        );
        assert_eq!(harness.layer.len(harness.window), 1);
        assert!(!first.is_open(harness.app.app()));
        assert_eq!(first_reason.get(), Some(DialogClose::Dismissed));
    }

    #[test]
    fn sync_on_a_closed_dialog_is_a_no_op() {
        let mut harness = Harness::new();
        let dialog = harness.build(DialogBuilder::new("Closed").fade(false), "closed");
        assert!(
            !dialog.sync(harness.app.app_mut()).expect("同步"),
            "未打开的对话框不做事"
        );
        assert!(dialog.overlay_node().is_none());
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
        let _ = dialog.sync(harness.app.app_mut()).expect("收尾");
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(
            reason.get(),
            Some(DialogClose::Dismissed),
            "Esc = 引擎默认 Dismiss"
        );
        assert_eq!(ok_runs.get(), 0);
        assert_eq!(
            cancel_runs.get(),
            0,
            "Esc 是引擎立即卸载：不跑 alert 的 cancel_action（无应用层关闭钩子）"
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
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert_eq!(confirms.get(), 1, "第一次点击跑确认回调");
        assert!(dialog.is_closing(), "第一次点击开始淡出");
        harness.click_node(ok);
        assert_eq!(
            confirms.get(),
            1,
            "淡出中的第二次点击不得再跑用户回调（D2）"
        );
        for _ in 0..24 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let _ = harness.app.drain().expect("帧");
            if !dialog.is_open(harness.app.app()) {
                break;
            }
        }
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
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        assert!(dialog.close(harness.app.app_mut()));
        assert!(dialog.is_closing(), "进入淡出");
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("关闭中重开"),
            "关闭中 open 必须真的重开（不得返回 Ok(false) 静默丢弃）"
        );
        assert!(dialog.is_open(harness.app.app()));
        assert!(!dialog.is_closing());
        assert_eq!(closes.get(), 1, "上一次关闭必须先收尾（on_close 派发一次）");
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        harness.settle(&dialog);
        let panel = dialog.panel_node().expect("重开后的面板");
        assert!(harness.tree_has(panel), "重开必须有新面板");
        assert_eq!(harness.layer.len(harness.window), 1, "旧浮层不得残留");
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
            "fade=false 已卸载：状态在帧边界更新"
        );
        assert!(dialog.is_closing());
        assert!(
            dialog
                .open(harness.app.app_mut(), harness.window)
                .expect("未收尾时重开"),
            "未收尾的关闭必须先收尾再重开"
        );
        assert!(dialog.is_open(harness.app.app()));
        assert!(!dialog.is_closing());
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
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
        let ok = dialog
            .confirm_button()
            .expect("确认按钮")
            .node()
            .expect("节点");
        harness.click_node(ok);
        assert!(dialog.is_closing());
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
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
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
        let wrapper = dialog.overlay_node().expect("包装节点");
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
        harness.app.advance(Duration::from_millis(200));
        let _ = harness.app.drain().expect("淡入帧");
        let _ = dialog.sync(harness.app.app_mut()).expect("同步");
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
        assert!(dialog.is_closing(), "OK 动作里的 close 开始淡出");
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
        for _ in 0..24 {
            harness.app.advance(Duration::from_millis(16));
            let _ = dialog.sync(harness.app.app_mut()).expect("同步");
            let _ = harness.app.drain().expect("帧");
            if !dialog.is_open(harness.app.app()) {
                break;
            }
        }
        assert!(!dialog.is_open(harness.app.app()));
        assert_eq!(reason.get(), Some(DialogClose::Dismissed));
        assert_eq!(ok_runs.get(), 1, "整段淡出只跑一次 OK 动作");
    }
}
