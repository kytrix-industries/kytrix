use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSApplication, NSAutoresizingMaskOptions, NSBackingStoreType, NSButton, NSColor,
    NSFloatingWindowLevel, NSModalPanelWindowLevel, NSScreen, NSView, NSWindow, NSWindowButton,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowOcclusionState, NSWindowOrderingMode,
    NSWindowStyleMask, NSWindowTabbingMode, NSWindowTitleVisibility,
};
use objc2_foundation::{
    NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_quartz_core::CAMetalLayer;
use smol_str::SmolStr;

use crate::{
    app::App,
    error::EngineError,
    event::{Event, ImeEvent},
    math::{DevicePixelRatio, LogicalPoint, LogicalRect, LogicalSize, px},
    paint::style::Cursor,
    platform::{
        dispatcher::Dispatcher,
        macos::{a11y::MacA11y, dpi, menu, view::GuiView, wake},
        macos_chrome::{self, LightRect},
        macos_close::{CloseAction, CloseBridge},
        macos_owner::{
            ModalOwnerChromes, ModalOwnerRelease, OwnerChrome, WindowKeyInput, WindowLevel,
        },
        macos_placement as placement,
        menu::MenuCommand,
    },
    window::{WindowControl, WindowId, WindowOptions, WindowState},
};

struct WindowIvars {
    close: Rc<CloseBridge>,
    moved: Cell<bool>,
    dispatcher: Arc<Dispatcher>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "KytrixWindowDelegate"]
    #[ivars = WindowIvars]
    struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            if self.ivars().close.request() {
                self.ivars().dispatcher.wake();
            }
            false
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            if self.ivars().close.will_close() {
                self.ivars().dispatcher.wake();
            }
        }

        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn window_did_change_occlusion_state(&self, _notification: &NSNotification) {
            self.ivars().dispatcher.wake();
        }

        #[unsafe(method(windowDidMiniaturize:))]
        fn window_did_miniaturize(&self, _notification: &NSNotification) {
            self.ivars().dispatcher.wake();
        }

        #[unsafe(method(windowDidDeminiaturize:))]
        fn window_did_deminiaturize(&self, _notification: &NSNotification) {
            self.ivars().dispatcher.wake();
        }

        #[unsafe(method(windowDidMove:))]
        fn window_did_move(&self, _notification: &NSNotification) {
            self.ivars().moved.set(true);
            self.ivars().dispatcher.wake();
        }
    }
);

impl WindowDelegate {
    fn new(
        mtm: MainThreadMarker,
        close: Rc<CloseBridge>,
        dispatcher: Arc<Dispatcher>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowIvars {
            close,
            moved: Cell::new(false),
            dispatcher,
        });
        unsafe { msg_send![super(this), init] }
    }

    fn take_moved(&self) -> bool {
        self.ivars().moved.replace(false)
    }
}

pub(crate) const MIN_SIZE_LOGICAL: LogicalSize = LogicalSize::new(px(320.0), px(200.0));

pub(crate) const fn unsupported_window_control(
    control: crate::window::WindowControl,
) -> Option<&'static str> {
    use crate::window::WindowControl;
    match control {
        WindowControl::Minimize
        | WindowControl::ToggleMaximize
        | WindowControl::ToggleFullscreen => None,
        WindowControl::BeginMove => Some("interactive move window control"),
        WindowControl::BeginResize(_) => Some("interactive resize window control"),
    }
}

fn screen_areas(mtm: MainThreadMarker) -> Vec<placement::ScreenArea> {
    let screens = NSScreen::screens(mtm);
    let mut areas = Vec::with_capacity(screens.count());
    for screen in screens.to_vec() {
        areas.push(placement::ScreenArea::new(
            ns_area(screen.frame()),
            ns_area(screen.visibleFrame()),
        ));
    }
    areas
}

fn ns_area(rect: NSRect) -> placement::Area {
    placement::Area::new(
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

fn read_owner_chrome(window: &NSWindow) -> OwnerChrome {
    let enabled = |button: NSWindowButton| {
        window
            .standardWindowButton(button)
            .is_some_and(|button| button.isEnabled())
    };
    OwnerChrome {
        ignores_mouse_events: window.ignoresMouseEvents(),
        movable_by_window_background: window.isMovableByWindowBackground(),
        close_button_enabled: enabled(NSWindowButton::CloseButton),
        minimize_button_enabled: enabled(NSWindowButton::MiniaturizeButton),
        zoom_button_enabled: enabled(NSWindowButton::ZoomButton),
    }
}

fn apply_owner_chrome(window: &NSWindow, chrome: OwnerChrome) {
    window.setIgnoresMouseEvents(chrome.ignores_mouse_events);
    window.setMovableByWindowBackground(chrome.movable_by_window_background);
    for (button, enabled) in [
        (NSWindowButton::CloseButton, chrome.close_button_enabled),
        (
            NSWindowButton::MiniaturizeButton,
            chrome.minimize_button_enabled,
        ),
        (NSWindowButton::ZoomButton, chrome.zoom_button_enabled),
    ] {
        if let Some(button) = window.standardWindowButton(button) {
            button.setEnabled(enabled);
        }
    }
}

fn hand_key_to_another_modal(owner: &NSWindow, closing: &NSWindow) {
    let Some(children) = owner.childWindows() else {
        return;
    };
    for child in children.to_vec() {
        if std::ptr::eq(&*child, closing) {
            continue;
        }
        if child.level() != NSModalPanelWindowLevel || !child.isVisible() {
            continue;
        }
        child.makeKeyWindow();
        return;
    }
}

fn cascade_content_origin(
    mtm: MainThreadMarker,
    screens: &[placement::ScreenArea],
    size: LogicalSize,
    index: usize,
) -> (f64, f64) {
    let Some(desktop) = placement::Desktop::from_screens(screens) else {
        return (80.0, 80.0);
    };
    let main_visible = NSScreen::mainScreen(mtm)
        .map(|screen| ns_area(screen.visibleFrame()))
        .unwrap_or(screens[0].visible());
    let position = placement::cascade_position(desktop, main_visible, index);
    placement::content_origin_for_position(position, size, screens)
}

pub(crate) struct OwnerWindow {
    pub id: WindowId,
    pub window: Retained<NSWindow>,
}

pub(crate) struct NativeWindow {
    mtm: MainThreadMarker,
    id: WindowId,
    window: Retained<NSWindow>,
    view: Retained<GuiView>,
    delegate: Retained<WindowDelegate>,
    close: Rc<CloseBridge>,
    owner: Option<OwnerWindow>,
    modal: bool,
    modal_registry: Rc<RefCell<ModalOwnerChromes>>,
    scale: DevicePixelRatio,
    logical_size: LogicalSize,
    seen_scale: f64,
    seen_frame_rate: f32,
    seen_size: (f64, f64),
    was_key: bool,
    visible: bool,
    destroyed: bool,
    ime_area: Option<LogicalRect>,
    title: SmolStr,
    traffic_light_position: Option<LogicalPoint>,
    fullscreen: bool,
    traffic_light_inset_cache: Option<LogicalPoint>,
    traffic_light_inset_refresh: bool,
    ime_enabled: bool,
    focus_lost: bool,
    seen_content: placement::Area,
}

impl NativeWindow {
    pub(crate) fn create(
        mtm: MainThreadMarker,
        id: WindowId,
        options: &WindowOptions,
        dispatcher: &Arc<Dispatcher>,
        cascade_index: usize,
        a11y_consumer_epoch: crate::a11y::adapter::A11yConsumerEpoch,
        modal_registry: Rc<RefCell<ModalOwnerChromes>>,
    ) -> Result<Self, EngineError> {
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
            | NSWindowStyleMask::FullSizeContentView;
        let size = LogicalSize::new(
            options.size.width.max(px(1.0)),
            options.size.height.max(px(1.0)),
        );
        let screens = screen_areas(mtm);
        let (content_x, content_y) = match options.position.filter(|position| position.is_finite())
        {
            Some(position) => placement::content_origin_for_position(position, size, &screens),
            None => cascade_content_origin(mtm, &screens, size, cascade_index),
        };
        let rect = NSRect::new(
            NSPoint::new(content_x, content_y),
            NSSize::new(
                f64::from(size.width.value()),
                f64::from(size.height.value()),
            ),
        );
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(options.title.as_str()));
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        window.setOpaque(true);
        let [red, green, blue, _] = crate::render::color::background_srgb(options.background);
        let background = NSColor::colorWithSRGBRed_green_blue_alpha(
            f64::from(red),
            f64::from(green),
            f64::from(blue),
            1.0,
        );
        window.setBackgroundColor(Some(&background));
        window.setAcceptsMouseMovedEvents(true);
        let (min_size, max_size) =
            placement::content_size_bounds(options.min_size, options.max_size, MIN_SIZE_LOGICAL);
        window.setContentMinSize(NSSize::new(
            f64::from(min_size.width.value()),
            f64::from(min_size.height.value()),
        ));
        if let Some(max_size) = max_size {
            window.setContentMaxSize(NSSize::new(
                f64::from(max_size.width.value()),
                f64::from(max_size.height.value()),
            ));
        }
        window.setTabbingMode(NSWindowTabbingMode::Disallowed);

        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);

        let close = Rc::new(CloseBridge::new());
        let view = GuiView::new(mtm, Arc::clone(dispatcher), Rc::clone(&close));
        window.setContentView(Some(&view));
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view.setWantsLayer(true);
        window.makeFirstResponder(Some(&view));
        if let Some(button) = window.standardWindowButton(NSWindowButton::CloseButton) {
            let target: &AnyObject = unsafe { &*(&*view as *const GuiView).cast::<AnyObject>() };
            unsafe { button.setTarget(Some(target)) };
            unsafe { button.setAction(Some(menu::selector(MenuCommand::Close))) };
        }

        let a11y = MacA11y::new(&view, dispatcher.clone(), id, a11y_consumer_epoch)?;
        view.install_a11y(a11y);

        let delegate = WindowDelegate::new(mtm, Rc::clone(&close), Arc::clone(dispatcher));
        let protocol: &ProtocolObject<dyn NSWindowDelegate> = ProtocolObject::from_ref(&*delegate);
        window.setDelegate(Some(protocol));
        let mut native = Self {
            mtm,
            id,
            window,
            view,
            delegate,
            close,
            owner: None,
            modal: false,
            modal_registry,
            scale: options.scale,
            logical_size: options.size,
            seen_scale: 1.0,
            seen_frame_rate: 0.0,
            seen_size: (0.0, 0.0),
            was_key: false,
            visible: true,
            destroyed: false,
            ime_area: None,
            title: options.title.clone(),
            traffic_light_position: None,
            fullscreen: false,
            traffic_light_inset_cache: None,
            traffic_light_inset_refresh: true,
            ime_enabled: true,
            focus_lost: false,
            seen_content: placement::Area::new(0.0, 0.0, 0.0, 0.0),
        };
        native.sync_initial(mtm)?;
        Ok(native)
    }

    fn sync_initial(&mut self, _mtm: MainThreadMarker) -> Result<(), EngineError> {
        let factor = self.window.backingScaleFactor();
        let scale = dpi::scale_from(factor)?;
        self.scale = scale;
        self.seen_scale = factor;
        let size = self.view.frame().size;
        self.seen_size = (size.width, size.height);
        self.logical_size = LogicalSize::new(px(size.width as f32), px(size.height as f32));
        self.seen_content = self.content_rect_in_screen();
        Ok(())
    }

    fn content_rect_in_screen(&self) -> placement::Area {
        ns_area(self.window.convertRectToScreen(self.view.frame()))
    }

    pub(crate) fn attach_layer(&self, layer: Retained<CAMetalLayer>) {
        self.view.setLayer(Some(&layer));
        let bounds = self.view.bounds();
        crate::render::macos::swapchain::set_layer_geometry(
            &layer,
            (bounds.size.width, bounds.size.height),
        );
    }

    pub(crate) fn show(&self) {
        self.window.makeKeyAndOrderFront(None);
        self.window.makeFirstResponder(Some(&self.view));
    }

    pub(crate) fn id(&self) -> WindowId {
        self.id
    }

    pub(crate) fn window_handle(&self) -> Retained<NSWindow> {
        self.window.clone()
    }

    pub(crate) fn key_input(&self) -> WindowKeyInput {
        WindowKeyInput {
            window: self.id,
            owner: self.owner.as_ref().map(|owner| owner.id),
            modal: self.modal,
            is_key: self.window.isKeyWindow(),
        }
    }

    pub(crate) fn make_key(&self) {
        self.window.makeKeyWindow();
    }

    pub(crate) fn apply_owner(&mut self, owner: Option<OwnerWindow>, level: WindowLevel) {
        match level {
            WindowLevel::Normal => {}
            WindowLevel::Floating => self.window.setLevel(NSFloatingWindowLevel),
            WindowLevel::ModalPanel => self.window.setLevel(NSModalPanelWindowLevel),
        }
        if let Some(owner) = owner {
            unsafe {
                owner
                    .window
                    .addChildWindow_ordered(&self.window, NSWindowOrderingMode::Above)
            };
            self.owner = Some(owner);
        }
        if matches!(level, WindowLevel::ModalPanel) {
            self.block_modal_owner();
        }
    }

    fn block_modal_owner(&mut self) {
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        let observed = read_owner_chrome(&owner.window);
        let first = self
            .modal_registry
            .borrow_mut()
            .open(self.id, owner.id, observed);
        self.modal = true;
        if first {
            apply_owner_chrome(&owner.window, OwnerChrome::BLOCKED);
        }
    }

    fn restore_modal_owner(&mut self) {
        if !self.modal {
            return;
        }
        let owner_usable = self
            .owner
            .as_ref()
            .is_some_and(|owner| owner.window.isVisible() && !owner.window.isMiniaturized());
        debug_assert!(
            self.owner.is_some(),
            "modal 窗口在 restore 前必须仍持有 owner（detach_owner 只在其后调用）"
        );
        let release = self.modal_registry.borrow_mut().close(
            self.id,
            self.window.isKeyWindow(),
            owner_usable,
        );
        self.modal = false;
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        match release {
            ModalOwnerRelease::Last { chrome, return_key } => {
                apply_owner_chrome(&owner.window, chrome);
                if return_key {
                    owner.window.makeKeyWindow();
                }
            }
            ModalOwnerRelease::Remaining { hand_key } => {
                if hand_key {
                    hand_key_to_another_modal(&owner.window, &self.window);
                }
            }
            ModalOwnerRelease::None => {}
        }
    }

    fn detach_owner(&mut self) {
        if let Some(owner) = self.owner.take() {
            owner.window.removeChildWindow(&self.window);
        }
    }

    pub(crate) fn scale(&self) -> DevicePixelRatio {
        self.scale
    }

    pub(crate) fn logical_size(&self) -> LogicalSize {
        self.logical_size
    }

    pub(crate) fn backing_size(&self) -> (u32, u32) {
        let backing = self.view.convertSizeToBacking(self.view.bounds().size);
        (
            (backing.width.round() as u32).max(1),
            (backing.height.round() as u32).max(1),
        )
    }

    pub(crate) fn maximum_frames_per_second(&self) -> f32 {
        self.window
            .screen()
            .map_or(60.0, |screen| screen.maximumFramesPerSecond() as f32)
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    pub(crate) fn take_events(&self) -> Vec<Event> {
        self.view.take_events()
    }

    pub(crate) fn sync_state(&mut self, app: &mut App) -> Result<bool, EngineError> {
        let mut events: Vec<Event> = Vec::new();
        let refresh = self.maximum_frames_per_second();
        if (refresh - self.seen_frame_rate).abs() > 0.5 {
            self.seen_frame_rate = refresh;
            if let Some(target) = app.window_mut(self.id) {
                target.set_max_frame_rate(refresh);
            }
        }
        let backing = self.backing_size();
        let factor = self.window.backingScaleFactor();
        if (factor - self.seen_scale).abs() > f64::EPSILON {
            let scale = dpi::scale_from(factor)?;
            self.seen_scale = factor;
            self.scale = scale;
            if let Some(window) = app.window_mut(self.id) {
                window.set_scale(scale);
                window.set_surface(backing);
            }
            events.push(Event::DpiChanged {
                dpi: dpi::dpi_from_scale(scale),
                scale,
            });
        }
        let size = self.view.frame().size;
        if (size.width - self.seen_size.0).abs() > 0.5
            || (size.height - self.seen_size.1).abs() > 0.5
        {
            self.seen_size = (size.width, size.height);
            let logical = LogicalSize::new(px(size.width as f32), px(size.height as f32));
            self.logical_size = logical;
            if let Some(window) = app.window_mut(self.id) {
                window.set_size(logical);
                window.set_surface(backing);
            }
            events.push(Event::Resized { size: logical });
        }
        let content = self.content_rect_in_screen();
        let appkit_moved = self.delegate.take_moved();
        if appkit_moved
            || (content.x() - self.seen_content.x()).abs() > 0.5
            || (content.max_y() - self.seen_content.max_y()).abs() > 0.5
        {
            self.seen_content = content;
            let screens = screen_areas(self.mtm);
            let position = placement::position_from_content_rect(content, &screens);
            events.push(Event::Moved { position });
        }
        let key = self.window.isKeyWindow();
        if key != self.was_key {
            self.was_key = key;
            self.view.focus_changed(key);
            events.push(Event::FocusChanged { focused: key });
            if !key {
                self.focus_lost = true;
            }
        }
        let minimized = self.window.isMiniaturized();
        let app_hidden = NSApplication::sharedApplication(self.mtm).isHidden();
        let occluded = !self
            .window
            .occlusionState()
            .contains(NSWindowOcclusionState::Visible);
        let visibility_changed = self.sync_visibility(app, !minimized && !app_hidden && !occluded);
        let changed = !events.is_empty() || visibility_changed;
        for event in events {
            app.dispatch(self.id, event);
        }
        Ok(changed)
    }

    fn sync_visibility(&mut self, app: &mut App, visible: bool) -> bool {
        if visible == self.visible {
            return false;
        }
        self.visible = visible;
        if !visible {
            return true;
        }
        let backing = self.backing_size();
        if let Some(window) = app.window_mut(self.id) {
            window.set_surface(backing);
            window.request_repaint();
        }
        true
    }

    pub(crate) fn sync_chrome(&mut self, app: &mut App) -> bool {
        let Some(window) = app.window(self.id) else {
            return false;
        };
        let desired_title = window.options().title.clone();
        let desired_position = window.traffic_light_position();
        let mut changed = false;
        if desired_title != self.title {
            self.window
                .setTitle(&NSString::from_str(desired_title.as_str()));
            self.title = desired_title;
        }
        if let Some(target) = macos_chrome::traffic_light_position_to_apply(
            self.traffic_light_position,
            desired_position,
        ) {
            self.traffic_light_position = Some(target);
            self.apply_traffic_light_position();
            self.traffic_light_inset_refresh = true;
            changed = true;
        }
        let fullscreen = self
            .window
            .styleMask()
            .contains(NSWindowStyleMask::FullScreen);
        if fullscreen != self.fullscreen {
            self.fullscreen = fullscreen;
            self.traffic_light_inset_refresh = true;
        }
        if macos_chrome::should_refresh_traffic_light_inset(
            fullscreen,
            self.traffic_light_inset_refresh,
            self.traffic_light_inset_cache,
        ) {
            let computed = self.traffic_light_inset();
            self.traffic_light_inset_refresh = computed.is_none();
            self.traffic_light_inset_cache = computed;
        }
        let state = WindowState {
            client_decoration: false,
            maximized: self.window.isZoomed(),
            fullscreen,
            can_maximize: true,
            can_minimize: true,
            can_fullscreen: true,
        };
        let inset = if fullscreen {
            None
        } else {
            self.traffic_light_inset_cache
        };
        let Some(window) = app.window_mut(self.id) else {
            return changed;
        };
        if window.set_state(state) {
            changed = true;
        }
        if window.set_traffic_light_inset(inset) {
            changed = true;
        }
        changed
    }

    pub(crate) fn apply_window_control(&self, control: WindowControl) {
        match control {
            WindowControl::Minimize => self.window.performMiniaturize(None),
            WindowControl::ToggleMaximize => self.window.performZoom(None),
            WindowControl::ToggleFullscreen => self.window.toggleFullScreen(None),
            WindowControl::BeginMove | WindowControl::BeginResize(_) => {
                debug_assert!(
                    false,
                    "unsupported window control reached the macOS pump: {control:?}"
                );
            }
        }
    }

    fn traffic_light_rects(&self) -> Vec<LightRect> {
        let Some(content) = self.window.contentView() else {
            return Vec::new();
        };
        let mut rects = Vec::with_capacity(3);
        for button in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            let Some(button) = self.window.standardWindowButton(button) else {
                continue;
            };
            let frame = button.convertRect_toView(button.bounds(), Some(&content));
            rects.push(LightRect {
                x: frame.origin.x,
                y: frame.origin.y,
                width: frame.size.width,
                height: frame.size.height,
            });
        }
        rects
    }

    fn traffic_light_inset(&self) -> Option<LogicalPoint> {
        macos_chrome::traffic_light_inset(&self.traffic_light_rects())
    }

    fn apply_traffic_light_position(&self) {
        let Some(target) = self.traffic_light_position else {
            return;
        };
        let Some(content) = self.window.contentView() else {
            return;
        };
        let mut entries: Vec<(Retained<NSButton>, Retained<NSView>, NSRect)> =
            Vec::with_capacity(3);
        for button in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            let Some(button) = self.window.standardWindowButton(button) else {
                continue;
            };
            let Some(superview) = (unsafe { button.superview() }) else {
                continue;
            };
            let frame = button.convertRect_toView(button.bounds(), Some(&content));
            entries.push((button, superview, frame));
        }
        let (Some(min_x), Some(min_y)) = (
            entries
                .iter()
                .map(|(_, _, frame)| frame.origin.x)
                .reduce(f64::min),
            entries
                .iter()
                .map(|(_, _, frame)| frame.origin.y)
                .reduce(f64::min),
        ) else {
            return;
        };
        let dx = f64::from(target.x.value()) - min_x;
        let dy = f64::from(target.y.value()) - min_y;
        if dx.abs() <= 0.5 && dy.abs() <= 0.5 {
            return;
        }
        for (button, superview, frame) in entries {
            let target_origin = NSPoint::new(frame.origin.x + dx, frame.origin.y + dy);
            let super_origin = content.convertPoint_toView(target_origin, Some(&superview));
            button.setFrameOrigin(super_origin);
        }
    }

    pub(crate) fn update_a11y(&mut self, app: &mut App) {
        if !app.a11y_pending(self.id) {
            return;
        }
        let id = self.id;
        self.view
            .update_a11y(|| crate::a11y::adapter::build_update(app, id));
    }

    pub(crate) fn sync_ime_cursor_area(&mut self, app: &App) {
        let area = match app.ime_cursor_area(self.id) {
            Ok(area) => area,
            Err(_) => return,
        };
        self.view
            .set_edit_availability(app.edit_menu_availability(area.is_some()));
        if self.ime_area == area {
            return;
        }
        self.ime_area = area;
        self.view.set_ime_cursor_area(area);
    }

    pub(crate) fn sync_ime_enabled(&mut self, app: &mut App) {
        let enabled = app.ime_enabled(self.id);
        if enabled == self.ime_enabled {
            return;
        }
        self.ime_enabled = enabled;
        if !enabled {
            app.clear_ime_cursor_area(self.id);
        }
        self.ime_area = None;
        if self.view.apply_ime_enabled(enabled) {
            app.dispatch(self.id, Event::Ime(ImeEvent::End));
        }
    }

    pub(crate) fn take_focus_lost(&mut self) -> bool {
        std::mem::take(&mut self.focus_lost)
    }

    pub(crate) fn handle_focus_lost(&mut self, app: &mut App) {
        app.clear_ime_cursor_area(self.id);
        self.ime_area = None;
        if self.view.reset_ime_composition() {
            app.dispatch(self.id, Event::Ime(ImeEvent::End));
        }
    }

    pub(crate) fn set_cursor(&self, cursor: Cursor) {
        self.view.set_cursor(cursor);
    }

    pub(crate) fn sync_drag_regions(&self, regions: &[LogicalRect]) {
        self.view.set_drag_regions(regions);
    }

    pub(crate) fn close(&mut self) {
        if self.destroyed {
            return;
        }
        self.view.uninstall_a11y();
        self.restore_modal_owner();
        self.detach_owner();
        self.window.setDelegate(None);
        self.window.close();
        self.destroyed = true;
    }

    pub(crate) fn decide_close(&self, verdict: impl FnOnce() -> bool) -> CloseAction {
        self.close.decide(verdict)
    }

    pub(crate) fn mark_externally_closed(&mut self) {
        if self.destroyed {
            return;
        }
        self.view.uninstall_a11y();
        self.restore_modal_owner();
        self.detach_owner();
        self.window.setDelegate(None);
        self.destroyed = true;
    }

    pub(crate) fn is_destroyed(&self) -> bool {
        self.destroyed
    }

    pub(crate) fn set_animating(&self, active: bool) {
        let refresh = self.maximum_frames_per_second().max(1.0);
        wake::set_animating(active, f64::from(1.0 / refresh));
    }

    pub(crate) fn set_wake_deadline(&self, deadline: Option<Instant>) {
        wake::set_deadline(deadline);
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        self.close();
    }
}
