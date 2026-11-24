use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use smol_str::SmolStr;
use windows::{
    Win32::{
        Foundation::{FALSE, HANDLE, HWND, LPARAM, RECT, TRUE},
        Graphics::Gdi::{
            DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, HBRUSH,
            MONITOR_DEFAULTTONEAREST, MONITORINFO, MONITORINFOEXW, MonitorFromWindow,
        },
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{Input::KeyboardAndMouse::EnableWindow, WindowsAndMessaging::*},
    },
    core::{BOOL, PCWSTR},
};

use crate::{
    app::App,
    error::EngineError,
    event::{Event, ImeEvent},
    math::{
        DevicePixelRatio, DevicePoint, DeviceRect, DeviceSize, LogicalPoint, LogicalPx,
        LogicalRect, LogicalSize, dp,
    },
    platform::{
        dispatcher::Dispatcher,
        windows::{
            chrome,
            composition::{self, Composition},
            dark_mode, dpi,
            event_loop::{ResizeFrameDriver, WndProcState, wndproc},
            icon, ime, placement,
        },
    },
    window::{Decorations, WindowControl, WindowId, WindowOptions, WindowState},
};

const CLASS_NAME: &str = "KytrixGuiWindow";

const MODAL_OWNER_PROPERTY: &str = "KytrixModalOwner";

static MODAL_PROPERTY_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

const DEFAULT_WINDOW_ORIGIN: (i32, i32) = (100, 100);

fn load_arrow_cursor() -> HCURSOR {
    unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default()
}

pub(crate) const fn unsupported_window_control(
    control: crate::window::WindowControl,
) -> Option<&'static str> {
    use crate::window::WindowControl;
    match control {
        WindowControl::Minimize | WindowControl::ToggleMaximize => None,
        WindowControl::ToggleFullscreen => Some("fullscreen window control"),
        WindowControl::BeginMove => Some("interactive move window control"),
        WindowControl::BeginResize(_) => Some("interactive resize window control"),
    }
}

fn refresh_hz_from_display_frequency(frequency: u32) -> Option<f32> {
    (frequency > 1).then_some(frequency as f32)
}

pub(crate) struct NativeWindow {
    id: WindowId,
    hwnd: HWND,
    composition: Arc<Mutex<Composition>>,
    state: *mut WndProcState,
    composition_geometry: Option<((u32, u32), (u32, u32))>,
    scale: DevicePixelRatio,
    logical_size: LogicalSize,
    ime_area: Option<(LogicalRect, DevicePixelRatio)>,
    decorations: Decorations,
    title: SmolStr,
    icon: Option<HICON>,
}

#[cfg(test)]
pub(crate) fn test_a11y_epoch() -> crate::a11y::adapter::A11yConsumerEpoch {
    crate::a11y::adapter::A11yBridge::new().consumer_epoch()
}

impl NativeWindow {
    pub(crate) fn create(
        id: WindowId,
        options: &WindowOptions,
        owner: Option<HWND>,
        dispatcher: Arc<Dispatcher>,
        app: Option<&Rc<RefCell<App>>>,
        a11y_consumer_epoch: crate::a11y::adapter::A11yConsumerEpoch,
    ) -> Result<Self, EngineError> {
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|error| window_error("GetModuleHandleW", error))?;
        register_class(instance)?;

        let class = wide(CLASS_NAME);
        let title = wide(&options.title);
        let (window_x, window_y, provisional_width, provisional_height) =
            provisional_window_rect(options, dpi::system_dpi());
        let ex_style = if options.always_on_top {
            WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOPMOST
        } else {
            WS_EX_NOREDIRECTIONBITMAP
        };
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                windows::core::PCWSTR(class.as_ptr()),
                windows::core::PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                window_x,
                window_y,
                provisional_width,
                provisional_height,
                owner,
                None,
                Some(instance.into()),
                None,
            )
        }
        .map_err(|error| window_error("CreateWindowExW", error))?;

        dark_mode::set_titlebar(hwnd, dark_mode::current().unwrap_or(false));

        let composition = match Composition::new(hwnd) {
            Ok(composition) => Arc::new(Mutex::new(composition)),
            Err(error) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        let driver = app.map(|app| {
            Rc::new(ResizeFrameDriver::new(
                Rc::downgrade(app),
                id,
                Arc::clone(&composition),
            ))
        });
        let limits = placement::ClientLimits::from_options(options);
        let state = match WndProcState::new(
            dispatcher,
            hwnd,
            id,
            limits,
            options.decorations,
            driver,
            a11y_consumer_epoch,
        ) {
            Ok(state) => Box::into_raw(Box::new(state)),
            Err(error) => {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
                return Err(error);
            }
        };
        if options.modal
            && let Some(owner_hwnd) = owner
        {
            let property = wide(MODAL_OWNER_PROPERTY);
            let marked =
                unsafe { SetPropW(hwnd, PCWSTR(property.as_ptr()), Some(HANDLE(owner_hwnd.0))) }
                    .is_ok();
            if marked {
                unsafe {
                    let _ = EnableWindow(owner_hwnd, false);
                }
            } else if !MODAL_PROPERTY_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "kytrix: SetPropW({MODAL_OWNER_PROPERTY}) failed; this modal window carries no \
                     native mark, so its owner is not disabled natively (the engine still filters \
                     the owner's input as modal)"
                );
            }
        }

        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };

        if options.decorations == Decorations::None {
            let _ = chrome::apply_frameless_dwm(hwnd);
        }

        let mut window = Self {
            id,
            hwnd,
            composition,
            state,
            composition_geometry: None,
            scale: DevicePixelRatio::ONE,
            logical_size: options.size,
            ime_area: None,
            decorations: options.decorations,
            title: options.title.clone(),
            icon: None,
        };
        window.apply_dpi_and_size(options)?;
        if let Some(bytes) = options.icon.as_deref() {
            let bitmap = icon::decode(bytes)
                .map_err(|cause| EngineError::WindowIcon { window: id, cause })?;
            let handle = icon::create(&bitmap)?;
            icon::apply_to_window(hwnd, handle);
            window.icon = Some(handle);
        }
        Ok(window)
    }

    pub(crate) fn composition(&self) -> Arc<Mutex<Composition>> {
        Arc::clone(&self.composition)
    }

    pub(crate) fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    pub(crate) fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub(crate) fn id(&self) -> WindowId {
        self.id
    }

    pub(crate) fn refresh_hz(&self) -> Option<f32> {
        let monitor = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if !unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool() {
            return None;
        }
        let mut devmode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !unsafe {
            EnumDisplaySettingsW(
                windows::core::PCWSTR(info.szDevice.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut devmode,
            )
        }
        .as_bool()
        {
            return None;
        }
        refresh_hz_from_display_frequency(devmode.dmDisplayFrequency)
    }

    pub(crate) fn scale(&self) -> DevicePixelRatio {
        self.scale
    }

    pub(crate) fn logical_size(&self) -> LogicalSize {
        self.logical_size
    }

    pub(crate) fn composition_geometry(&self) -> Option<((u32, u32), (u32, u32))> {
        self.composition_geometry
    }

    pub(crate) fn note_composition_geometry(&mut self, geometry: ((u32, u32), (u32, u32))) {
        self.composition_geometry = Some(geometry);
    }

    pub(crate) fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        let ok = unsafe { GetClientRect(self.hwnd, &mut rect) };
        match ok {
            Ok(()) => (
                (rect.right - rect.left).max(1) as u32,
                (rect.bottom - rect.top).max(1) as u32,
            ),
            Err(_) => physical_client(self.logical_size, self.scale),
        }
    }

    pub(crate) fn is_minimized(&self) -> bool {
        unsafe { IsIconic(self.hwnd) }.as_bool()
    }

    pub(crate) fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNORMAL);
        }
    }

    fn apply_dpi_and_size(&mut self, options: &WindowOptions) -> Result<(), EngineError> {
        let requested = options.size;
        if requested.width <= LogicalPx::ZERO || requested.height <= LogicalPx::ZERO {
            let dpi = dpi::window_dpi(self.hwnd);
            let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
                factor: dpi as f32 / dpi::BASE_DPI as f32,
            })?;
            self.scale = scale;
            unsafe {
                (*self.state).note_dpi(dpi, scale);
            }
            self.logical_size = requested;
            return Ok(());
        }
        let size = self.state_mut().client_limits().clamp(requested);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let dpi = dpi::window_dpi(self.hwnd);
            let scale = dpi::scale_from_dpi(dpi).ok_or(EngineError::InvalidDpiScale {
                factor: dpi as f32 / dpi::BASE_DPI as f32,
            })?;
            self.scale = scale;
            unsafe {
                (*self.state).note_dpi(dpi, scale);
            }
            let client = physical_client(size, scale);
            let (width, height) = dpi::window_size_for_client(client, dpi, self.decorations);
            match options.position {
                Some(position) => {
                    let client_origin = dpi::physical_origin(position, scale);
                    let (x, y) =
                        dpi::window_origin_for_client(client_origin, client, dpi, self.decorations);
                    let target = DeviceRect::from_origin_size(
                        DevicePoint::new(dp(x), dp(y)),
                        DeviceSize::new(dp(width), dp(height)),
                    );
                    let placed = placement::clamp_to_work_area(target, &placement::work_areas());
                    unsafe {
                        SetWindowPos(
                            self.hwnd,
                            None,
                            placed.x0.value(),
                            placed.y0.value(),
                            placed.width().value(),
                            placed.height().value(),
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                        )
                    }
                    .map_err(|error| window_error("SetWindowPos", error))?;
                }
                None => {
                    unsafe {
                        SetWindowPos(
                            self.hwnd,
                            None,
                            0,
                            0,
                            width,
                            height,
                            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE | SWP_FRAMECHANGED,
                        )
                    }
                    .map_err(|error| window_error("SetWindowPos", error))?;
                }
            }
            if attempt >= 2 || dpi::window_dpi(self.hwnd) == dpi {
                break;
            }
        }
        let scale = self.scale;
        let client = self.client_size();
        self.note_client_resize(scale, client);
        Ok(())
    }

    pub(crate) fn state_mut(&mut self) -> &mut WndProcState {
        unsafe { &mut *self.state }
    }

    pub(crate) fn update_a11y(&mut self, app: &mut crate::app::App) {
        let window = self.id;
        if !app.a11y_pending(window) {
            return;
        }
        let events = {
            let state = self.state_mut();
            state
                .a11y_mut()
                .update(|| crate::a11y::adapter::build_update(app, window))
        };
        if let Some(events) = events {
            events.raise();
        }
    }

    pub(crate) fn sync_ime_cursor_area(&mut self, app: &App) {
        let area = match app.ime_cursor_area(self.id) {
            Ok(area) => area,
            Err(_) => return,
        };
        let next = area.map(|rect| (rect, self.scale));
        if self.ime_area == next {
            return;
        }
        self.ime_area = next;
        let hwnd = self.hwnd;
        let physical = area.map(|rect| self.scale.to_device_rect(rect));
        let command = self.state_mut().plan_ime_cursor_area(physical);
        if let Some(command) = command {
            let applied = ime::execute(hwnd, command);
            self.state_mut().note_ime_applied(applied);
        }
    }

    pub(crate) fn sync_ime_enabled(&mut self, app: &mut App) {
        let enabled = app.ime_enabled(self.id);
        if self.state_mut().ime_enabled() == enabled {
            return;
        }
        let was_composing = !enabled && self.state_mut().ime_composing();
        let hwnd = self.hwnd;
        let command = self.state_mut().plan_ime_enabled(enabled);
        if let Some(command) = command {
            let applied = ime::execute(hwnd, command);
            self.state_mut().note_ime_applied(applied);
        }
        self.ime_area = None;
        if !enabled {
            app.clear_ime_cursor_area(self.id);
            if was_composing {
                app.dispatch(self.id, Event::Ime(ImeEvent::End));
            }
        }
    }

    pub(crate) fn sync_drag_regions(&mut self, app: &App) {
        let Some(window) = app.window(self.id) else {
            return;
        };
        self.state_mut().set_drag_regions(window.drag_regions());
    }

    pub(crate) fn sync_chrome(&mut self, app: &mut App) {
        let Some(window) = app.window(self.id) else {
            return;
        };
        let desired_title = window.options().title.clone();
        if desired_title != self.title && set_window_title(self.hwnd, &desired_title) {
            self.title = desired_title;
        }
        if !self.state_mut().take_chrome_dirty() {
            return;
        }
        let state = self.current_state();
        let changed = app
            .window_mut(self.id)
            .is_some_and(|window| window.set_state(state));
        if changed {
            app.request_frame();
        }
    }

    fn current_state(&self) -> WindowState {
        WindowState {
            client_decoration: self.decorations == Decorations::None,
            maximized: unsafe { IsZoomed(self.hwnd) }.as_bool(),
            fullscreen: false,
            can_maximize: true,
            can_minimize: true,
            can_fullscreen: false,
        }
    }

    pub(crate) fn apply_window_control(&self, control: WindowControl) {
        match control {
            WindowControl::Minimize => unsafe {
                let _ = ShowWindow(self.hwnd, SW_MINIMIZE);
            },
            WindowControl::ToggleMaximize => {
                let command = unsafe {
                    if IsZoomed(self.hwnd).as_bool() {
                        SW_RESTORE
                    } else {
                        SW_MAXIMIZE
                    }
                };
                unsafe {
                    let _ = ShowWindow(self.hwnd, command);
                }
            }
            WindowControl::ToggleFullscreen
            | WindowControl::BeginMove
            | WindowControl::BeginResize(_) => {
                debug_assert!(
                    false,
                    "unsupported window control reached the Windows pump: {control:?}"
                );
            }
        }
    }

    pub(crate) fn is_destroyed(&self) -> bool {
        self.hwnd.is_invalid()
    }

    pub(crate) fn note_scale(&mut self, scale: DevicePixelRatio, dpi: u32) {
        self.scale = scale;
        self.state_mut().note_dpi(dpi, scale);
        let _ = self.state_mut().plan_ime_cursor_area(None);
        self.ime_area = None;
    }

    pub(crate) fn note_client_resize(&mut self, scale: DevicePixelRatio, client: (u32, u32)) {
        self.logical_size =
            scale.to_logical_size(DeviceSize::new(dp(client.0 as i32), dp(client.1 as i32)));
    }

    pub(crate) fn requery_display(&mut self, app: &mut App) {
        let dpi = dpi::window_dpi(self.hwnd);
        let Some(scale) = dpi::scale_from_dpi(dpi) else {
            return;
        };
        if scale != self.scale {
            self.note_scale(scale, dpi);
        }
        let Some(window) = app.window_mut(self.id) else {
            return;
        };
        if let Some(hz) = self.refresh_hz() {
            window.set_frame_refresh_hz(hz);
        }
        if window.scale() != scale {
            window.set_scale(scale);
        }
        if self.is_minimized() {
            return;
        }
        let client = self.client_size();
        self.note_client_resize(scale, client);
        window.set_size(self.logical_size);
        window.set_surface(client);
    }

    pub(crate) fn destroy(&mut self) {
        icon::destroy(self.icon.take());
        if !self.hwnd.is_invalid() {
            let property = wide(MODAL_OWNER_PROPERTY);
            let owner = unsafe { GetPropW(self.hwnd, PCWSTR(property.as_ptr())) };
            if !owner.is_invalid() {
                unsafe {
                    let _ = RemovePropW(self.hwnd, PCWSTR(property.as_ptr()));
                }
                let owner = HWND(owner.0);
                if !another_modal_window_for(owner, self.hwnd) {
                    unsafe {
                        let _ = EnableWindow(owner, true);
                    }
                }
            }
        }
        if self.hwnd.is_invalid() {
            return;
        }
        let hwnd = self.hwnd;
        let command = self.state_mut().plan_ime_release();
        if let Some(command) = command {
            let applied = ime::execute(hwnd, command);
            self.state_mut().note_ime_applied(applied);
        }
        composition::lock(&self.composition).shutdown();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        self.hwnd = HWND::default();
    }
}

impl Drop for NativeWindow {
    fn drop(&mut self) {
        self.destroy();
        if !self.state.is_null() {
            unsafe {
                drop(Box::from_raw(self.state));
            }
            self.state = std::ptr::null_mut();
        }
    }
}

struct ModalOwnerProbe {
    property: PCWSTR,
    owner: HWND,
    excluded: HWND,
    found: bool,
}

unsafe extern "system" fn probe_modal_owner(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let probe = unsafe { &mut *(lparam.0 as *mut ModalOwnerProbe) };
    if hwnd != probe.excluded && unsafe { GetPropW(hwnd, probe.property) } == HANDLE(probe.owner.0)
    {
        probe.found = true;
        return FALSE;
    }
    TRUE
}

fn another_modal_window_for(owner: HWND, excluded: HWND) -> bool {
    let property = wide(MODAL_OWNER_PROPERTY);
    let mut probe = ModalOwnerProbe {
        property: PCWSTR(property.as_ptr()),
        owner,
        excluded,
        found: false,
    };
    unsafe {
        let _ = EnumThreadWindows(
            GetCurrentThreadId(),
            Some(probe_modal_owner),
            LPARAM(std::ptr::from_mut(&mut probe) as isize),
        );
    }
    probe.found
}

fn provisional_window_rect(options: &WindowOptions, dpi: u32) -> (i32, i32, i32, i32) {
    let scale = dpi::scale_from_dpi(dpi).unwrap_or(DevicePixelRatio::ONE);
    let client = physical_client(options.size, scale);
    let (width, height) = dpi::window_size_for_client(client, dpi, options.decorations);
    let Some(position) = options.position else {
        return (
            DEFAULT_WINDOW_ORIGIN.0,
            DEFAULT_WINDOW_ORIGIN.1,
            width,
            height,
        );
    };
    let origin = dpi::window_origin_for_client(
        dpi::physical_origin(position, scale),
        client,
        dpi,
        options.decorations,
    );
    let target = DeviceRect::from_origin_size(
        DevicePoint::new(dp(origin.0), dp(origin.1)),
        DeviceSize::new(dp(width), dp(height)),
    );
    let placed = placement::clamp_to_work_area(target, &placement::work_areas());
    (
        placed.x0.value(),
        placed.y0.value(),
        placed.width().value(),
        placed.height().value(),
    )
}

fn physical_client(size: LogicalSize, scale: DevicePixelRatio) -> (u32, u32) {
    let rect =
        scale.to_covering_device_rect(LogicalRect::from_origin_size(LogicalPoint::ZERO, size));
    (
        rect.width().max(dp(1)).value() as u32,
        rect.height().max(dp(1)).value() as u32,
    )
}

fn register_class(instance: windows::Win32::Foundation::HMODULE) -> Result<(), EngineError> {
    let class = wide(CLASS_NAME);
    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance.into(),
        hIcon: HICON::default(),
        hCursor: load_arrow_cursor(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: windows::core::PCWSTR::null(),
        lpszClassName: windows::core::PCWSTR(class.as_ptr()),
    };
    let atom = unsafe { RegisterClassW(&wc) };
    if atom != 0 {
        return Ok(());
    }
    let error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
    if error == 1410 {
        return Ok(());
    }
    Err(EngineError::WindowCreate {
        detail: smol_str::format_smolstr!("RegisterClassW: GetLastError={error}"),
    })
}

fn window_error(stage: &'static str, error: windows::core::Error) -> EngineError {
    EngineError::WindowCreate {
        detail: smol_str::format_smolstr!("{stage}: {:#010X}", error.code().0 as u32),
    }
}

fn set_window_title(hwnd: HWND, title: &str) -> bool {
    let text = wide(title);
    match unsafe { SetWindowTextW(hwnd, PCWSTR(text.as_ptr())) } {
        Ok(()) => true,
        Err(error) => {
            if !TITLE_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "kytrix: SetWindowTextW failed: {error:?}; the window keeps its previous title \
                     (the engine retries on the next pump round)"
                );
            }
            false
        }
    }
}

static TITLE_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use windows::Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::Gdi::{
            GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
        },
        UI::{
            Input::{
                Ime::{ImmGetContext, ImmReleaseContext},
                KeyboardAndMouse::IsWindowEnabled,
            },
            WindowsAndMessaging::{
                GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetWindow, GetWindowLongPtrW,
                GetWindowRect, GetWindowTextW, ICON_BIG, IsWindow, IsZoomed, NCCALCSIZE_PARAMS,
                SW_HIDE, SW_MAXIMIZE, SW_RESTORE, SendMessageW, ShowWindow, WM_GETICON,
                WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION, WM_NCCALCSIZE, WS_CAPTION,
                WS_CHILD, WS_EX_TOPMOST, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
            },
        },
    };

    use super::{NativeWindow, provisional_window_rect, refresh_hz_from_display_frequency};
    use crate::{
        app::App,
        event::{Event, ImeEvent},
        math::{DevicePixelRatio, LogicalRect, LogicalSize, px},
        node::builder::NodeBuilder,
        platform::dispatcher::Dispatcher,
        text::TextStyle,
        window::{Decorations, WindowId, WindowOptions},
    };
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn sync_ime_enabled_applies_the_app_target_state_and_destroy_restores_the_context() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let a11y_epoch = app.window_a11y_epoch(id).expect("window exists");
        let mut native =
            NativeWindow::create(id, &options, None, Dispatcher::new(), None, a11y_epoch)
                .expect("NativeWindow::create");

        let baseline = unsafe { ImmGetContext(native.hwnd) };
        if !baseline.is_invalid() {
            unsafe {
                let _ = ImmReleaseContext(native.hwnd, baseline);
            }
        }

        native.ime_area = Some((
            LogicalRect::new(px(0.0), px(0.0), px(1.0), px(18.0)),
            DevicePixelRatio::ONE,
        ));
        native.sync_ime_enabled(&mut app);
        assert!(
            native.ime_area.is_some(),
            "目标状态未变化时不得清 caret 缓存"
        );

        app.set_ime_enabled(id, false);
        native.sync_ime_enabled(&mut app);
        assert!(
            !native.state_mut().ime_enabled(),
            "禁用态必须应用到平台状态"
        );
        assert!(native.ime_area.is_none(), "状态切换必须清 caret 缓存");
        assert!(
            unsafe { ImmGetContext(native.hwnd) }.is_invalid(),
            "禁用后窗口不得再关联输入上下文"
        );

        app.set_ime_enabled(id, true);
        native.sync_ime_enabled(&mut app);
        assert!(native.state_mut().ime_enabled(), "启用态必须应用到平台状态");
        let restored = unsafe { ImmGetContext(native.hwnd) };
        assert!(!restored.is_invalid(), "重新启用必须恢复输入上下文");
        if !baseline.is_invalid() {
            assert_eq!(restored, baseline, "恢复的必须是被摘下的那个上下文");
        }
        unsafe {
            let _ = ImmReleaseContext(native.hwnd, restored);
        }

        app.set_ime_enabled(id, false);
        native.sync_ime_enabled(&mut app);
        assert!(!native.state_mut().ime_enabled(), "前置：窗口处于禁用态");
        native.destroy();
        assert!(
            native.state_mut().ime_enabled(),
            "destroy 必须先恢复 IME 关联状态，否则 HIMC 泄漏"
        );
    }

    #[test]
    fn requery_display_writes_the_live_scale_and_client_size_back() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let a11y_epoch = app.window_a11y_epoch(id).expect("window exists");
        let mut native =
            NativeWindow::create(id, &options, None, Dispatcher::new(), None, a11y_epoch)
                .expect("NativeWindow::create");

        {
            let window = app.window_mut(id).expect("window");
            window.set_scale(DevicePixelRatio::new(3.0).expect("scale"));
            window.set_size(LogicalSize::new(px(1.0), px(1.0)));
        }
        native.requery_display(&mut app);

        let window = app.window(id).expect("window");
        assert_eq!(
            window.device_pixel_ratio(),
            native.scale(),
            "恢复必须重新查询显示器 DPI 并写回比例"
        );
        assert_eq!(
            window.size(),
            native.logical_size(),
            "恢复必须重新查询客户区并写回逻辑尺寸"
        );
    }

    #[test]
    fn a_display_mode_frequency_becomes_a_refresh_hint() {
        assert_eq!(refresh_hz_from_display_frequency(60), Some(60.0));
        assert_eq!(refresh_hz_from_display_frequency(144), Some(144.0));
        assert_eq!(
            refresh_hz_from_display_frequency(0),
            None,
            "0 = 硬件默认，不可用"
        );
        assert_eq!(
            refresh_hz_from_display_frequency(1),
            None,
            "1 = 驱动决定，不可用"
        );
    }

    #[test]
    fn a_real_window_reports_a_plausible_refresh_rate() {
        let options = WindowOptions::default();
        let native = NativeWindow::create(
            crate::window::WindowId::for_test(1),
            &options,
            None,
            Dispatcher::new(),
            None,
            crate::a11y::adapter::A11yBridge::new().consumer_epoch(),
        )
        .expect("NativeWindow::create");
        match native.refresh_hz() {
            None => {}
            Some(hz) => {
                assert!(
                    (24.0..=480.0).contains(&hz),
                    "刷新率提示必须是真实量级：{hz}"
                );
                assert!(
                    hz.fract() == 0.0,
                    "EnumDisplaySettings 的频率是整数 Hz：{hz}"
                );
            }
        }
    }

    fn window(id: u32) -> NativeWindow {
        NativeWindow::create(
            WindowId::for_test(id),
            &WindowOptions::default(),
            None,
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("NativeWindow::create")
    }

    fn modal_window(id: u32, owner: &NativeWindow) -> NativeWindow {
        let options = WindowOptions {
            owner: Some(owner.id()),
            modal: true,
            ..WindowOptions::default()
        };
        NativeWindow::create(
            WindowId::for_test(id),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("modal window")
    }

    #[test]
    fn an_owned_window_reports_its_owner_and_is_not_a_child_window() {
        let owner = window(1);
        let options = WindowOptions {
            owner: Some(WindowId::for_test(1)),
            ..WindowOptions::default()
        };
        let owned = NativeWindow::create(
            WindowId::for_test(2),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("owned window");

        let reported = unsafe { GetWindow(owned.hwnd, GW_OWNER) }.expect("GetWindow(GW_OWNER)");
        assert_eq!(
            reported, owner.hwnd,
            "owned 窗口的 GW_OWNER 必须回指 owner（hWndParent owner 语义）"
        );
        let style = unsafe { GetWindowLongPtrW(owned.hwnd, GWL_STYLE) };
        assert_eq!(
            style & (WS_CHILD.0 as isize),
            0,
            "owner 语义不得退化成客户区子窗口（window_set/origin 行为会完全不同）"
        );
    }

    #[test]
    fn an_always_on_top_window_has_the_topmost_ex_style() {
        let options = WindowOptions {
            always_on_top: true,
            ..WindowOptions::default()
        };
        let topmost = NativeWindow::create(
            WindowId::for_test(3),
            &options,
            None,
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("topmost window");
        let ex_style = unsafe { GetWindowLongPtrW(topmost.hwnd, GWL_EXSTYLE) };
        assert_ne!(
            ex_style & (WS_EX_TOPMOST.0 as isize),
            0,
            "always_on_top 窗口必须带 WS_EX_TOPMOST"
        );

        let plain = window(4);
        let plain_ex_style = unsafe { GetWindowLongPtrW(plain.hwnd, GWL_EXSTYLE) };
        assert_eq!(
            plain_ex_style & (WS_EX_TOPMOST.0 as isize),
            0,
            "普通窗口不得带 WS_EX_TOPMOST"
        );
    }

    #[test]
    fn a_modal_window_disables_its_owner_until_it_is_destroyed() {
        let owner = window(5);
        let options = WindowOptions {
            owner: Some(WindowId::for_test(5)),
            modal: true,
            ..WindowOptions::default()
        };
        let mut modal = NativeWindow::create(
            WindowId::for_test(6),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("modal window");
        assert!(
            !unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "模态窗口打开期间 owner 必须被系统禁用（EnableWindow(FALSE)）"
        );

        modal.destroy();
        assert!(
            unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "模态窗口销毁后必须恢复 owner 输入（EnableWindow(TRUE)）"
        );
    }

    #[test]
    fn closing_one_of_two_modal_windows_keeps_the_owner_disabled() {
        let owner = window(11);
        let mut first = modal_window(12, &owner);
        let _second = modal_window(13, &owner);
        assert!(
            !unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "前置：两个模态窗口打开时 owner 必须被系统禁用"
        );

        first.destroy();
        assert!(
            !unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "关闭两个模态窗口中的一个后 owner 必须仍然禁用（EnableWindow 不是引用计数）"
        );
    }

    #[test]
    fn closing_the_last_modal_window_restores_the_owner() {
        let owner = window(14);
        let mut first = modal_window(15, &owner);
        let mut second = modal_window(16, &owner);

        second.destroy();
        assert!(
            !unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "先关较新的模态窗口时 owner 必须仍然禁用（较旧的模态窗口还活着）"
        );

        first.destroy();
        assert!(
            unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "最后一个模态窗口销毁后必须恢复 owner 输入"
        );
    }

    #[test]
    fn another_owners_modal_window_does_not_change_this_owners_state() {
        let first_owner = window(17);
        let second_owner = window(18);
        let mut first_modal = modal_window(19, &first_owner);
        let _second_modal = modal_window(20, &second_owner);
        assert!(
            !unsafe { IsWindowEnabled(first_owner.hwnd) }.as_bool()
                && !unsafe { IsWindowEnabled(second_owner.hwnd) }.as_bool(),
            "前置：两个 owner 都被各自的模态窗口禁用"
        );

        first_modal.destroy();
        assert!(
            unsafe { IsWindowEnabled(first_owner.hwnd) }.as_bool(),
            "本 owner 已没有活模态窗口时必须恢复（别的 owner 的模态窗口不算）"
        );
        assert!(
            !unsafe { IsWindowEnabled(second_owner.hwnd) }.as_bool(),
            "另一个 owner 的模态窗口必须继续禁用那个 owner"
        );
    }

    #[test]
    fn an_owned_window_without_modal_leaves_its_owner_enabled() {
        let owner = window(7);
        let options = WindowOptions {
            owner: Some(WindowId::for_test(7)),
            ..WindowOptions::default()
        };
        let _owned = NativeWindow::create(
            WindowId::for_test(8),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("owned window");
        assert!(
            unsafe { IsWindowEnabled(owner.hwnd) }.as_bool(),
            "非模态的 owned 窗口不得禁用 owner"
        );
    }

    #[test]
    fn destroying_the_owner_takes_the_owned_window_with_it() {
        let owner = window(9);
        let options = WindowOptions {
            owner: Some(WindowId::for_test(9)),
            ..WindowOptions::default()
        };
        let mut owned = NativeWindow::create(
            WindowId::for_test(10),
            &options,
            Some(owner.hwnd),
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("owned window");
        let owned_hwnd = owned.hwnd;

        drop(owner);
        assert!(
            !unsafe { IsWindow(Some(owned_hwnd)) }.as_bool(),
            "owner 销毁时系统必须一并销毁 owned 窗口（owner 语义契约）"
        );

        owned.destroy();
    }

    #[test]
    fn destroying_the_owner_with_a_live_modal_window_is_safe() {
        let owner = window(21);
        let mut modal = modal_window(22, &owner);
        let modal_hwnd = modal.hwnd;

        drop(owner);
        assert!(
            !unsafe { IsWindow(Some(modal_hwnd)) }.as_bool(),
            "owner 销毁时系统必须一并销毁其模态窗口"
        );
        modal.destroy();
    }

    #[test]
    fn disabling_the_ime_during_composition_emits_one_end_and_clears_the_caret_hint() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions::default();
        let id = app.create_window(options.clone());
        let node = {
            let window = app.window_mut(id).expect("窗口存在");
            let root = window.tree().root();
            NodeBuilder::text("abc", TextStyle::new(px(16.0)))
                .build(window.tree_mut(), root)
                .expect("文本节点")
        };
        let _ = app.refresh_text(id).expect("窗口存在").expect("文本 pass");
        let _ = app
            .window_mut(id)
            .expect("窗口存在")
            .compute_layout()
            .expect("布局");
        let _ = app.layout_text(id).expect("窗口存在").expect("文本重排");
        app.set_ime_cursor_area(id, node, 1).expect("caret hint");
        assert!(
            app.ime_cursor_area(id).expect("窗口存在").is_some(),
            "前置：文本 shape 后必须有 caret hint"
        );

        let mut native = NativeWindow::create(
            id,
            &options,
            None,
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("NativeWindow::create");
        let hwnd = native.hwnd;
        unsafe {
            SendMessageW(hwnd, WM_IME_STARTCOMPOSITION, None, None);
        }

        app.set_ime_enabled(id, false);
        native.sync_ime_enabled(&mut app);

        assert_eq!(
            app.ime_cursor_area(id).expect("窗口存在"),
            None,
            "禁用 IME 必须清应用侧 caret hint"
        );
        let events: Vec<Event> = app
            .drain_events()
            .into_iter()
            .filter(|(window, _)| *window == id)
            .map(|(_, event)| event)
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Ime(ImeEvent::End)))
                .count(),
            1,
            "禁用组合中的 IME 必须补恰好一条 ImeEvent::End（实际 {events:?}）"
        );

        unsafe {
            SendMessageW(hwnd, WM_IME_ENDCOMPOSITION, None, None);
        }
        assert!(
            !app.drain_events()
                .iter()
                .any(|(window, event)| *window == id && matches!(event, Event::Ime(ImeEvent::End))),
            "系统迟到的 WM_IME_ENDCOMPOSITION 不得重复投递 End"
        );
    }

    fn window_with_options(id: u32, options: &WindowOptions) -> NativeWindow {
        NativeWindow::create(
            WindowId::for_test(id),
            options,
            None,
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("NativeWindow::create")
    }

    fn window_title(hwnd: HWND) -> String {
        let mut buffer = [0u16; 256];
        let length = unsafe { GetWindowTextW(hwnd, &mut buffer) };
        String::from_utf16_lossy(&buffer[..usize::try_from(length).unwrap_or(0)])
    }

    fn png_icon() -> Vec<u8> {
        use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

        let rgba = [0xFFu8, 0x00, 0x00, 0xFF].repeat(16 * 16);
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(&rgba, 16, 16, ExtendedColorType::Rgba8)
            .expect("PNG 编码");
        png
    }

    #[test]
    fn provisional_window_rect_uses_the_given_dpi() {
        let options = WindowOptions {
            decorations: Decorations::None,
            size: LogicalSize::new(px(200.0), px(100.0)),
            ..WindowOptions::default()
        };
        let (_, _, width_96, height_96) = provisional_window_rect(&options, 96);
        let (_, _, width_192, height_192) = provisional_window_rect(&options, 192);
        assert_eq!((width_96, height_96), (200, 100));
        assert_eq!(
            (width_192, height_192),
            (400, 200),
            "the provisional client area must follow the passed DPI"
        );
    }
    #[test]
    fn a_frameless_window_keeps_the_overlapped_window_style() {
        let options = WindowOptions {
            decorations: Decorations::None,
            ..WindowOptions::default()
        };
        let native = window_with_options(31, &options);
        let style = unsafe { GetWindowLongPtrW(native.hwnd, GWL_STYLE) } as u32;
        for flag in [
            WS_CAPTION,
            WS_SYSMENU,
            WS_THICKFRAME,
            WS_MINIMIZEBOX,
            WS_MAXIMIZEBOX,
        ] {
            assert_ne!(
                style & flag.0,
                0,
                "frameless 窗口必须保留 {flag:?}（系统手势 / DWM 的入场券）"
            );
        }
    }

    #[test]
    fn wm_nccalcsize_makes_the_frameless_client_area_cover_the_whole_window() {
        let frameless = window_with_options(
            32,
            &WindowOptions {
                decorations: Decorations::None,
                ..WindowOptions::default()
            },
        );
        let mut window_rect = RECT::default();
        unsafe { GetWindowRect(frameless.hwnd, &mut window_rect) }.expect("GetWindowRect");
        let mut params = NCCALCSIZE_PARAMS {
            rgrc: [window_rect, window_rect, window_rect],
            lppos: std::ptr::null_mut(),
        };
        let result = unsafe {
            SendMessageW(
                frameless.hwnd,
                WM_NCCALCSIZE,
                Some(WPARAM(1)),
                Some(LPARAM((&raw mut params) as isize)),
            )
        };
        assert_eq!(result.0, 0, "frameless 的 WM_NCCALCSIZE 必须返回 0");
        assert_eq!(
            (
                params.rgrc[0].left,
                params.rgrc[0].top,
                params.rgrc[0].right,
                params.rgrc[0].bottom
            ),
            (
                window_rect.left,
                window_rect.top,
                window_rect.right,
                window_rect.bottom
            ),
            "frameless 客户区必须铺满整个窗口（否则屏幕上仍有一条系统标题栏）"
        );

        let native = window(33);
        let mut native_window = RECT::default();
        unsafe { GetWindowRect(native.hwnd, &mut native_window) }.expect("GetWindowRect");
        let mut native_client = RECT::default();
        unsafe { GetClientRect(native.hwnd, &mut native_client) }.expect("GetClientRect");
        assert!(
            native_window.right - native_window.left > native_client.right - native_client.left
                && native_window.bottom - native_window.top
                    > native_client.bottom - native_client.top,
            "Native 窗口必须保留系统非客户区（默认路径回归）：window={native_window:?} \
             client={native_client:?}"
        );
    }

    #[test]
    fn a_frameless_window_client_is_exactly_the_requested_size() {
        let options = WindowOptions {
            decorations: Decorations::None,
            size: LogicalSize::new(px(480.0), px(320.0)),
            ..WindowOptions::default()
        };
        let native = window_with_options(34, &options);
        let dpi = crate::platform::windows::dpi::window_dpi(native.hwnd);
        let scale = crate::platform::windows::dpi::scale_from_dpi(dpi).expect("scale");
        let client = native.client_size();
        assert_eq!(
            client,
            (
                (480.0 * scale.factor()).round() as u32,
                (320.0 * scale.factor()).round() as u32
            ),
            "frameless 的客户区必须正好是请求尺寸（dpi={dpi}）"
        );
        let mut window_rect = RECT::default();
        unsafe { GetWindowRect(native.hwnd, &mut window_rect) }.expect("GetWindowRect");
        let mut client_rect = RECT::default();
        unsafe { GetClientRect(native.hwnd, &mut client_rect) }.expect("GetClientRect");
        assert_eq!(
            (
                window_rect.right - window_rect.left,
                window_rect.bottom - window_rect.top
            ),
            (
                client_rect.right - client_rect.left,
                client_rect.bottom - client_rect.top
            ),
            "frameless 的窗口矩形必须等于客户区"
        );
    }

    #[test]
    fn sync_chrome_applies_the_runtime_title_and_reports_window_state() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions {
            title: smol_str::SmolStr::new_static("before"),
            decorations: Decorations::None,
            ..WindowOptions::default()
        };
        let id = app.create_window(options.clone());
        let mut native = NativeWindow::create(
            WindowId::for_test(id.to_bits()),
            &options,
            None,
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("NativeWindow::create");
        native.sync_chrome(&mut app);
        assert_eq!(window_title(native.hwnd), "before");

        let state = app.window(id).expect("窗口存在").state();
        assert!(state.client_decoration, "frameless ⇒ 应用自绘装饰");
        assert!(!state.maximized, "新建窗口不是最大化");
        assert!(!state.can_fullscreen, "Windows 没有全屏控制");

        app.set_title(id, smol_str::SmolStr::new_static("after"))
            .expect("标题必须被接受");
        native.sync_chrome(&mut app);
        assert_eq!(window_title(native.hwnd), "after");
        native.sync_chrome(&mut app);
        assert_eq!(
            window_title(native.hwnd),
            "after",
            "同值同步不得反复写标题（幂等）"
        );
    }

    #[test]
    fn a_maximized_frameless_window_matches_the_work_area() {
        let options = WindowOptions {
            decorations: Decorations::None,
            ..WindowOptions::default()
        };
        let native = window_with_options(35, &options);
        unsafe {
            let _ = ShowWindow(native.hwnd, SW_MAXIMIZE);
        }
        assert!(
            unsafe { IsZoomed(native.hwnd) }.as_bool(),
            "前置：ShowWindow(SW_MAXIMIZE) 后窗口必须真的最大化"
        );

        let mut client = RECT::default();
        unsafe { GetClientRect(native.hwnd, &mut client) }.expect("GetClientRect");
        let monitor = unsafe { MonitorFromWindow(native.hwnd, MONITOR_DEFAULTTONEAREST) };
        let mut info = MONITORINFO {
            cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).expect("MONITORINFO size"),
            ..Default::default()
        };
        assert!(
            unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool(),
            "GetMonitorInfoW"
        );
        let client_size = (client.right - client.left, client.bottom - client.top);
        let work_size = (
            info.rcWork.right - info.rcWork.left,
            info.rcWork.bottom - info.rcWork.top,
        );
        assert_eq!(
            client_size, work_size,
            "最大化客户区必须逐像素等于工作区（多/少一像素都是边框补偿错误；\
             client={client:?} work={:?}）",
            info.rcWork
        );

        unsafe {
            let _ = ShowWindow(native.hwnd, SW_RESTORE);
            let _ = ShowWindow(native.hwnd, SW_HIDE);
        }
    }

    #[test]
    fn maximizing_a_window_reports_the_state_through_sync_chrome() {
        let runtime = runtime();
        let mut app = App::new(runtime.handle().clone());
        let options = WindowOptions {
            decorations: Decorations::None,
            ..WindowOptions::default()
        };
        let id = app.create_window(options.clone());
        let dispatcher = Dispatcher::new();
        let mut native = NativeWindow::create(
            WindowId::for_test(id.to_bits()),
            &options,
            None,
            Arc::clone(&dispatcher),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .expect("NativeWindow::create");

        let _ = dispatcher.take_frame_wake();
        let _ = app.take_wake_request();
        native.sync_chrome(&mut app);
        assert!(
            !app.take_wake_request(),
            "首轮对账不得因初值校正而请求一帧（Windows 的初始 WindowState 必须已是真值）"
        );
        assert!(
            !app.window(id).expect("窗口存在").state().maximized,
            "前置：窗口未最大化"
        );

        unsafe {
            let _ = ShowWindow(native.hwnd, SW_MAXIMIZE);
        }
        let _ = dispatcher.take_frame_wake();
        let _ = app.take_wake_request();
        native.sync_chrome(&mut app);

        assert!(
            app.window(id).expect("窗口存在").state().maximized,
            "WM_SIZE 之后 sync_chrome 必须把 IsZoomed 写回 WindowState.maximized（P2-3）"
        );
        assert!(
            app.take_wake_request(),
            "状态变化必须请求一帧（§8.6.4 / I3）"
        );

        unsafe {
            let _ = ShowWindow(native.hwnd, SW_RESTORE);
            let _ = ShowWindow(native.hwnd, SW_HIDE);
        }
    }

    #[test]
    fn a_window_icon_is_applied_to_the_native_window() {
        let options = WindowOptions {
            icon: Some(Bytes::from(png_icon())),
            ..WindowOptions::default()
        };
        let native = window_with_options(36, &options);
        for slot in [ICON_BIG, 0u32] {
            let icon =
                unsafe { SendMessageW(native.hwnd, WM_GETICON, Some(WPARAM(slot as usize)), None) };
            assert_ne!(icon.0, 0, "WM_SETICON 槽 {slot} 必须拿回非空 HICON");
        }
    }

    #[test]
    fn an_invalid_icon_fails_window_creation_with_a_strong_error() {
        let options = WindowOptions {
            icon: Some(Bytes::from_static(b"definitely not a png or ico")),
            ..WindowOptions::default()
        };
        let error = NativeWindow::create(
            WindowId::for_test(37),
            &options,
            None,
            Dispatcher::new(),
            None,
            crate::platform::windows::window::test_a11y_epoch(),
        )
        .err()
        .expect("非法图标必须让建窗失败");
        assert!(
            matches!(error, crate::error::EngineError::WindowIcon { .. }),
            "必须是强类型 WindowIcon 错误：{error:?}"
        );
    }
}
