use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use windows::Win32::{
    Foundation::HWND,
    System::SystemInformation::GetTickCount64,
    UI::{
        Input::KeyboardAndMouse::{GetCapture, GetDoubleClickTime, ReleaseCapture, SetCapture},
        WindowsAndMessaging::*,
    },
};

use super::native_window::{
    apply_display_recovery, collect_events, resolve_owner, sync_native_windows,
};
use crate::{
    app::App,
    error::EngineError,
    platform::{
        dispatcher::Dispatcher,
        windows::{
            composition::{self, CompositionState},
            dpi_awareness,
            window::NativeWindow,
        },
    },
    render::backend::Renderer,
    window::{Window, WindowId, WindowOptions},
};

pub(super) const A11Y_TIMER_ID: usize = 1;

pub(super) const DEADLINE_TIMER_ID: usize = 2;

fn deadline_timer_millis(deadline: Instant, now: Instant) -> u32 {
    let nanos = deadline.saturating_duration_since(now).as_nanos();
    let millis = nanos.saturating_add(999_999) / 1_000_000;
    u32::try_from(millis.min(u128::from(u32::MAX)))
        .unwrap_or(u32::MAX)
        .max(1)
}

const A11Y_RETRY_MS: u32 = 100;

const FALLBACK_PACE: Duration = Duration::from_millis(16);

const MAX_PACE_WAIT_MS: u64 = 100;

const PACE_STOP_SLICE_MS: u64 = 20;

fn pace_interval(app: &App, windows: &[NativeWindow]) -> Duration {
    pace_interval_from(
        windows
            .first()
            .and_then(|native| app.window(native.id()))
            .and_then(Window::frame_refresh_hz),
    )
}

fn pace_interval_from(refresh_hz: Option<f32>) -> Duration {
    refresh_hz.map_or(FALLBACK_PACE, |hz| {
        Duration::from_secs_f32(1.0 / hz).min(Duration::from_millis(MAX_PACE_WAIT_MS))
    })
}

pub(super) fn install_input_thresholds(app: &mut App) {
    let (double_click, double_x, double_y, drag_x, drag_y) = unsafe {
        (
            GetDoubleClickTime(),
            GetSystemMetrics(SM_CXDOUBLECLK),
            GetSystemMetrics(SM_CYDOUBLECLK),
            GetSystemMetrics(SM_CXDRAG),
            GetSystemMetrics(SM_CYDRAG),
        )
    };
    let defaults = crate::interaction::InputThresholds::platform_default();
    let double_click_time = if double_click == 0 {
        defaults.double_click_time
    } else {
        std::time::Duration::from_millis(u64::from(double_click))
    };
    app.set_input_thresholds(crate::interaction::InputThresholds {
        double_click_time,
        double_click_distance: positive(double_x.max(double_y))
            .unwrap_or(defaults.double_click_distance),
        drag_distance: positive(drag_x.max(drag_y)).unwrap_or(defaults.drag_distance),
    });
}

fn positive(value: i32) -> Option<f32> {
    (value > 0).then_some(value as f32)
}

pub(super) fn wheel_lines_from_system() -> f32 {
    let mut lines: u32 = 3;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&raw mut lines).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() || lines == u32::MAX {
        return 0.0;
    }
    lines as f32
}

pub(super) fn message_ticks() -> u32 {
    unsafe { GetMessageTime() as u32 }
}

pub(super) fn tick_now() -> u32 {
    unsafe { GetTickCount64() as u32 }
}

pub(super) fn cursor_handle(cursor: crate::paint::style::Cursor) -> HCURSOR {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDC_HAND, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENS, IDC_SIZEWE,
    };

    use crate::paint::style::Cursor;

    let id = match cursor {
        Cursor::Default => IDC_ARROW,
        Cursor::Pointer => IDC_HAND,
        Cursor::Text => IDC_IBEAM,
        Cursor::NotAllowed => IDC_NO,
        Cursor::Grab | Cursor::Grabbing => IDC_SIZEALL,
        Cursor::ResizeHorizontal => IDC_SIZEWE,
        Cursor::ResizeVertical => IDC_SIZENS,
    };
    unsafe { LoadCursorW(None, id) }.unwrap_or_default()
}

pub(super) fn plan_release_capture(hwnd: HWND, state: &mut super::WndProcState) -> bool {
    if unsafe { GetCapture() } != hwnd {
        return false;
    }
    state.suppress_capture_cancel = true;
    true
}

enum InteractionAction {
    SetCursor(HCURSOR),
    SetCapture(HWND),
    ReleaseCapture(HWND),
}

fn apply_interaction_actions(actions: Vec<InteractionAction>) {
    for action in actions {
        match action {
            InteractionAction::SetCursor(cursor) => {
                unsafe { SetCursor(Some(cursor)) };
            }
            InteractionAction::SetCapture(hwnd) => {
                unsafe { SetCapture(hwnd) };
            }
            InteractionAction::ReleaseCapture(hwnd) => {
                debug_assert_eq!(
                    unsafe { GetCapture() },
                    hwnd,
                    "计划与执行之间捕获窗口变了：suppress / 释放的目标会错位"
                );
                let _ = unsafe { ReleaseCapture() };
            }
        }
    }
}

pub(super) struct FramePacer {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    interval: Duration,
    tick: Arc<AtomicBool>,
}

impl FramePacer {
    pub(super) fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            interval: FALLBACK_PACE,
            tick: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn sync(&mut self, active: bool, interval: Duration, dispatcher: &Arc<Dispatcher>) {
        if !active {
            self.shutdown();
            return;
        }
        if self.thread.is_some() && self.interval == interval {
            return;
        }
        self.shutdown();
        let stop = Arc::new(AtomicBool::new(false));
        let tick = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread_tick = Arc::clone(&tick);
        let thread_dispatcher = Arc::clone(dispatcher);
        self.thread = Some(std::thread::spawn(move || {
            pace(thread_stop, thread_tick, interval, thread_dispatcher);
        }));
        self.stop = stop;
        self.tick = tick;
        self.interval = interval;
    }

    pub(super) fn take_tick(&self) -> bool {
        self.tick.swap(false, Ordering::AcqRel)
    }

    pub(super) fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.interval = FALLBACK_PACE;
        self.tick.store(false, Ordering::Release);
    }
}

impl Default for FramePacer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FramePacer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn pace(
    stop: Arc<AtomicBool>,
    tick: Arc<AtomicBool>,
    interval: Duration,
    dispatcher: Arc<Dispatcher>,
) {
    let interval = interval.clamp(
        Duration::from_millis(1),
        Duration::from_millis(MAX_PACE_WAIT_MS),
    );
    let slice = Duration::from_millis(PACE_STOP_SLICE_MS);
    let mut next = Instant::now() + interval;
    loop {
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            if now >= next {
                break;
            }
            let remaining = next.saturating_duration_since(now);
            std::thread::sleep(remaining.min(slice));
        }
        if stop.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        next += interval;
        if next <= now {
            next = now + interval;
        }
        tick.store(true, Ordering::Release);
        dispatcher.wake();
    }
}

pub(crate) fn run(mut app: App) -> Result<(), EngineError> {
    crate::platform::windows::version::require_windows11()?;
    dpi_awareness::install()?;
    install_input_thresholds(&mut app);
    let dispatcher = app.dispatcher();
    dispatcher.bind_thread(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() });

    let mut requests: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    requests.sort_by_key(|(id, _)| id.to_bits());
    let app = Rc::new(RefCell::new(app));
    let mut windows: Vec<NativeWindow> = Vec::with_capacity(requests.len());
    for (id, options) in requests {
        let owner = resolve_owner(&options, &windows);
        let Some(a11y_epoch) = app.borrow().window_a11y_epoch(id) else {
            continue;
        };
        let mut native = NativeWindow::create(
            id,
            &options,
            owner,
            dispatcher.clone(),
            Some(&app),
            a11y_epoch,
        )?;
        let client = native.client_size();
        let renderer = Renderer::new_for_hwnd(native.composition(), client, options.background)?;
        let refresh_hz = native.refresh_hz();
        {
            let mut app = app.borrow_mut();
            if let Some(window) = app.window_mut(id) {
                window.set_renderer(renderer);
                window.set_scale(native.scale());
                window.set_size(native.logical_size());
                window.set_surface(client);
                if let Some(hz) = refresh_hz {
                    window.set_frame_refresh_hz(hz);
                }
            }
        }
        {
            let app = app.borrow();
            native.sync_drag_regions(&app);
        }
        {
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(client, client)?;
        }
        native.note_composition_geometry((client, client));
        native.show();
        windows.push(native);
    }
    if let Some(first) = windows.first() {
        dispatcher.attach(first.hwnd_raw());
        crate::platform::windows::clipboard::set_owner(first.hwnd_raw());
    }
    dispatcher.wake();
    pump(app, windows, dispatcher)
}

pub(super) fn pump(
    app: Rc<RefCell<App>>,
    mut windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let result = pump_loop(&app, &mut windows, &dispatcher);
    drop(app);
    drop(windows);
    result
}

#[derive(Clone, Copy, PartialEq)]
struct GeometryKey {
    logical: crate::math::Size,
    scale: crate::math::Scale,
}

enum ClientProbe {
    Minimized,
    Measured((u32, u32)),
}

struct GeometryCache {
    entries: Vec<GeometryEntry>,
    #[cfg(test)]
    queries: u64,
}

struct GeometryEntry {
    window: WindowId,
    key: GeometryKey,
    client: (u32, u32),
}

impl GeometryCache {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            #[cfg(test)]
            queries: 0,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn client(
        &mut self,
        window: WindowId,
        key: GeometryKey,
        probe: impl FnOnce() -> ClientProbe,
    ) -> Option<(u32, u32)> {
        if let Some(index) = self.entries.iter().position(|entry| entry.window == window)
            && self.entries[index].key == key
        {
            return Some(self.entries[index].client);
        }
        match probe() {
            ClientProbe::Minimized => None,
            ClientProbe::Measured(client) => {
                #[cfg(test)]
                {
                    self.queries += 1;
                }
                match self.entries.iter_mut().find(|entry| entry.window == window) {
                    Some(entry) => {
                        entry.key = key;
                        entry.client = client;
                    }
                    None => self.entries.push(GeometryEntry {
                        window,
                        key,
                        client,
                    }),
                }
                Some(client)
            }
        }
    }

    fn retain_live(&mut self, live: impl Fn(WindowId) -> bool) {
        self.entries.retain(|entry| live(entry.window));
    }

    #[cfg(test)]
    const fn queries(&self) -> u64 {
        self.queries
    }
}

fn composition_health_plan(
    seen: &mut u64,
    epoch: u64,
    ids: impl Iterator<Item = WindowId>,
    mut check: impl FnMut(WindowId) -> CompositionState,
) -> Vec<WindowId> {
    if epoch == *seen {
        return Vec::new();
    }
    *seen = epoch;
    ids.filter(|id| check(*id) == CompositionState::Invalid)
        .collect()
}

pub(super) fn pump_loop(
    app_cell: &Rc<RefCell<App>>,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let mut message = MSG::default();
    let mut pacer = FramePacer::new();
    let mut geometry = GeometryCache::new();
    let mut a11y_timer: Option<WindowId> = None;
    let mut deadline_timer: Option<WindowId> = None;
    let mut composition_epoch_seen = composition::device_check_epoch();
    loop {
        for native in windows.iter_mut() {
            native.state_mut().end_message_batch();
        }
        loop {
            let has_message = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) };
            if !has_message.as_bool() {
                break;
            }
            if message.message == WM_QUIT {
                return Ok(());
            }
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }

        {
            let mut app = app_cell.borrow_mut();
            app.sync_clock_live();
            if sync_native_windows(&mut app, app_cell, windows, dispatcher)? {
                retarget_wake(windows, dispatcher);
            }
            collect_events(&mut app, windows, dispatcher)?;
        }
        {
            let mut app = app_cell.borrow_mut();
            apply_display_recovery(&mut app, windows, dispatcher);
            app.poll_renderers()?;
        }
        let applied_control = {
            let pending_control = {
                let mut app = app_cell.borrow_mut();
                app.take_window_control()
            };
            let target = pending_control.and_then(|(id, control)| {
                windows
                    .iter()
                    .find(|native| native.id() == id && !native.is_destroyed())
                    .map(|native| (native, control))
            });
            if let Some((native, control)) = target {
                native.apply_window_control(control);
                true
            } else {
                false
            }
        };
        {
            let mut app = app_cell.borrow_mut();
            if applied_control {
                collect_events(&mut app, windows, dispatcher)?;
            }
            for native in windows.iter_mut() {
                native.sync_chrome(&mut app);
            }
        }
        for native in windows.iter_mut() {
            if let Some(error) = native.state_mut().take_pending_error() {
                return Err(error);
            }
        }
        let plan = composition_health_plan(
            &mut composition_epoch_seen,
            composition::device_check_epoch(),
            windows.iter().map(NativeWindow::id),
            |id| match windows.iter().find(|native| native.id() == id) {
                Some(native) => {
                    let target = native.composition();
                    composition::lock(&target).check_device_state()
                }
                None => CompositionState::WindowClosed,
            },
        );
        if !plan.is_empty() {
            let mut app = app_cell.borrow_mut();
            for id in plan {
                if let Some(window) = app.window_mut(id) {
                    window.request_composition_rebuild();
                }
            }
        }
        let app_shared = app_cell.borrow();
        for native in windows.iter_mut() {
            let id = native.id();
            let key = GeometryKey {
                logical: native.logical_size(),
                scale: native.scale(),
            };
            let Some(client) = geometry.client(id, key, || {
                if native.is_minimized() {
                    ClientProbe::Minimized
                } else {
                    ClientProbe::Measured(native.client_size())
                }
            }) else {
                continue;
            };
            let presented = app_shared
                .window(id)
                .map(Window::presented_surface)
                .unwrap_or(client);
            let pending = app_shared
                .window(id)
                .is_some_and(Window::take_composition_pending);
            if !pending && native.composition_geometry() == Some((presented, client)) {
                continue;
            }
            let target = native.composition();
            let mut target = composition::lock(&target);
            target.set_geometry(presented, client)?;
            native.note_composition_geometry((presented, client));
        }
        drop(app_shared);
        if geometry.len() > windows.len() {
            geometry.retain_live(|id| windows.iter().any(|native| native.id() == id));
        }

        let mut actions: Vec<InteractionAction> = Vec::with_capacity(4);
        {
            let mut app = app_cell.borrow_mut();
            if let Some((target, cursor)) = app.take_cursor_change()
                && let Some(native) = windows.iter_mut().find(|native| native.id() == target)
            {
                native.state_mut().set_cursor(cursor);
                actions.push(InteractionAction::SetCursor(cursor_handle(cursor)));
            }
            for (target, capture) in app.take_capture_requests() {
                let Some(native) = windows.iter_mut().find(|native| native.id() == target) else {
                    continue;
                };
                let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
                if capture {
                    actions.push(InteractionAction::SetCapture(hwnd));
                } else if plan_release_capture(hwnd, native.state_mut()) {
                    actions.push(InteractionAction::ReleaseCapture(hwnd));
                }
            }
        }
        apply_interaction_actions(actions);

        let (continuous, deferred, a11y_pending) = {
            let app = app_cell.borrow();
            (
                app.needs_continuous_frames(),
                app.has_deferred_frames(),
                app.a11y_retry_pending(),
            )
        };
        if !windows.is_empty() {
            let interval = {
                let app = app_cell.borrow();
                pace_interval(&app, windows)
            };
            pacer.sync(continuous || deferred, interval, dispatcher);
            let native = windows.first_mut().expect("上面已检查非空");
            let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
            if a11y_timer.is_some_and(|owner| owner != native.id()) {
                a11y_timer = None;
            }
            match (a11y_pending, a11y_timer) {
                (true, None) => {
                    let id = unsafe { SetTimer(Some(hwnd), A11Y_TIMER_ID, A11Y_RETRY_MS, None) };
                    if id != 0 {
                        a11y_timer = Some(native.id());
                    }
                }
                (false, Some(_)) => {
                    let _ = unsafe { KillTimer(Some(hwnd), A11Y_TIMER_ID) };
                    a11y_timer = None;
                }
                _ => {}
            }
        } else {
            pacer.sync(false, FALLBACK_PACE, dispatcher);
        }

        {
            let mut app = app_cell.borrow_mut();
            if close_requested_windows(&mut app, windows, dispatcher)? {
                return Ok(());
            }
        }

        {
            let mut app = app_cell.borrow_mut();
            app.run_frame_hook();
        }
        let tick = pacer.take_tick();
        if dispatcher.take_frame_wake() {
            let mut app = app_cell.borrow_mut();
            frame_all(&mut app, windows, tick)?;
        }

        {
            let mut app = app_cell.borrow_mut();
            for native in windows.iter() {
                if native.is_minimized()
                    && let Some(window) = app.window_mut(native.id())
                {
                    window.clear_frame_budget_deferred();
                }
            }
        }
        let (continuous, deferred) = {
            let app = app_cell.borrow();
            (app.needs_continuous_frames(), app.has_deferred_frames())
        };
        if windows.is_empty() {
            pacer.sync(false, FALLBACK_PACE, dispatcher);
        } else {
            let interval = {
                let app = app_cell.borrow();
                pace_interval(&app, windows)
            };
            pacer.sync(continuous || deferred, interval, dispatcher);
        }

        {
            let mut app = app_cell.borrow_mut();
            for native in windows.iter_mut() {
                native.update_a11y(&mut app);
                native.sync_drag_regions(&app);
                native.sync_ime_enabled(&mut app);
                native.sync_ime_cursor_area(&app);
            }
        }
        app_cell
            .borrow_mut()
            .wake_for_a11y_backlog(dispatcher.as_ref());

        for task in dispatcher.drain_tasks() {
            let mut app = app_cell.borrow_mut();
            task.run(&mut app);
        }

        {
            let desired = {
                let app = app_cell.borrow();
                app.next_wake_deadline()
            };
            if let Some(host) = deadline_timer.take()
                && let Some(native) = windows
                    .iter()
                    .find(|native| native.id() == host && !native.is_destroyed())
            {
                let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
                let _ = unsafe { KillTimer(Some(hwnd), DEADLINE_TIMER_ID) };
            }
            if let Some(deadline) = desired
                && let Some(native) = windows.iter().find(|native| !native.is_destroyed())
            {
                let millis = deadline_timer_millis(deadline, Instant::now());
                let hwnd = HWND(native.hwnd_raw() as *mut core::ffi::c_void);
                let id = unsafe { SetTimer(Some(hwnd), DEADLINE_TIMER_ID, millis, None) };
                if id != 0 {
                    deadline_timer = Some(native.id());
                }
            }
        }

        let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
        if result.0 == 0 {
            return Ok(());
        }
        if result.0 == -1 {
            let last_error = unsafe { windows::Win32::Foundation::GetLastError() }.0;
            return Err(EngineError::WindowCreate {
                detail: smol_str::format_smolstr!("GetMessageW: GetLastError={last_error}"),
            });
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

pub(super) fn close_requested_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<bool, EngineError> {
    let mut closed = false;
    for native in windows.iter_mut() {
        if native.state_mut().take_close_request() {
            let id = native.id();
            if !app.request_close(id) {
                continue;
            }
            native.destroy();
            closed = true;
        }
    }
    if closed {
        windows.retain(|native| !native.is_destroyed());
        retarget_wake(windows, dispatcher);
    }
    Ok(windows.is_empty() || app.windows().next().is_none())
}

pub(super) fn retarget_wake(windows: &[NativeWindow], dispatcher: &Arc<Dispatcher>) {
    match windows.first() {
        Some(native) => {
            dispatcher.attach(native.hwnd_raw());
            crate::platform::windows::clipboard::set_owner(native.hwnd_raw());
        }
        None => dispatcher.detach(),
    }
}

pub(super) fn frame_all(
    app: &mut App,
    windows: &[NativeWindow],
    force: bool,
) -> Result<(), EngineError> {
    for native in windows {
        let outcome = if force {
            app.frame_forced(native.id())
        } else {
            app.frame(native.id())
        };
        match outcome {
            None => {}
            Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod geometry_cache_tests {
    use super::{ClientProbe, GeometryCache, GeometryKey};
    use crate::{
        math::{Scale, Size},
        window::WindowId,
    };

    fn window(raw: u32) -> WindowId {
        WindowId::from_raw(raw)
    }

    fn key(width: f32) -> GeometryKey {
        GeometryKey {
            logical: Size::new(width, 600.0),
            scale: Scale::ONE,
        }
    }

    fn measured(client: (u32, u32)) -> impl Fn() -> ClientProbe {
        move || ClientProbe::Measured(client)
    }

    #[test]
    fn a_steady_pump_iteration_queries_the_client_size_zero_times() {
        let mut cache = GeometryCache::new();
        let id = window(1);
        let mut probes = 0u32;
        assert_eq!(
            cache.client(id, key(800.0), || {
                probes += 1;
                ClientProbe::Measured((800, 600))
            }),
            Some((800, 600))
        );
        for _ in 0..1_000 {
            let client = cache.client(id, key(800.0), || {
                probes += 1;
                ClientProbe::Measured((800, 600))
            });
            assert_eq!(client, Some((800, 600)), "缓存命中必须给出上次的客户区");
        }
        assert_eq!(probes, 1, "稳态 1000 轮只允许 1 次查询（实际 {probes} 次）");
        assert_eq!(cache.queries(), 1, "查询计数必须停在 1");
    }

    #[test]
    fn a_resize_or_dpi_change_queries_the_client_size_again() {
        let mut cache = GeometryCache::new();
        let id = window(1);
        let probes = std::cell::Cell::new(0u32);
        let probe = |client: (u32, u32)| {
            probes.set(probes.get() + 1);
            ClientProbe::Measured(client)
        };
        assert_eq!(
            cache.client(id, key(800.0), || probe((800, 600))),
            Some((800, 600))
        );
        assert_eq!(
            cache.client(id, key(900.0), || probe((900, 700))),
            Some((900, 700))
        );
        let mut dpi = key(900.0);
        dpi.scale = Scale::new(1.25).expect("scale");
        assert_eq!(
            cache.client(id, dpi, || probe((1125, 875))),
            Some((1125, 875))
        );
        assert_eq!(
            probes.get(),
            3,
            "最后一次查询必须跟上新键（实际 {} 次）",
            probes.get()
        );
        assert_eq!(cache.queries(), 3);
        assert_eq!(
            cache.client(id, key(800.0), || probe((800, 600))),
            Some((800, 600))
        );
        assert_eq!(probes.get(), 4);
    }

    #[test]
    fn a_minimized_window_is_never_cached() {
        let mut cache = GeometryCache::new();
        let id = window(1);
        assert_eq!(
            cache.client(id, key(800.0), || ClientProbe::Measured((800, 600))),
            Some((800, 600))
        );
        let mut probes = 0u32;
        for _ in 0..3 {
            assert_eq!(
                cache.client(id, key(900.0), || {
                    probes += 1;
                    ClientProbe::Minimized
                }),
                None,
                "最小化时必须跳过几何应用"
            );
        }
        assert_eq!(probes, 3, "最小化状态不能被缓存（每轮重试）");
        assert_eq!(
            cache.client(id, key(900.0), measured((900, 700))),
            Some((900, 700))
        );
        let mut restored_probes = 0u32;
        assert_eq!(
            cache.client(id, key(900.0), || {
                restored_probes += 1;
                ClientProbe::Measured((900, 700))
            }),
            Some((900, 700))
        );
        assert_eq!(restored_probes, 0, "恢复后的稳态必须走缓存");
    }

    #[test]
    fn closed_windows_do_not_keep_cache_entries() {
        let mut cache = GeometryCache::new();
        for raw in 1..=3u32 {
            let id = window(raw);
            assert_eq!(
                cache.client(id, key(800.0), measured((800, 600))),
                Some((800, 600))
            );
        }
        assert_eq!(cache.len(), 3);
        cache.retain_live(|id| id == window(2));
        assert_eq!(cache.len(), 1, "关闭的窗口必须丢掉缓存项");
        assert_eq!(cache.queries(), 3);
    }
}

#[cfg(test)]
mod pacer_tests {
    use std::time::Duration;

    use super::{FALLBACK_PACE, MAX_PACE_WAIT_MS, pace_interval_from};

    #[test]
    fn the_pacer_interval_follows_the_output_refresh_hint() {
        assert_eq!(pace_interval_from(None), FALLBACK_PACE);
        let hz60 = pace_interval_from(Some(60.0));
        assert!(
            (hz60.as_secs_f64() - 1.0 / 60.0).abs() < 1e-6,
            "60 Hz 提示 ⇒ 16.67 ms 周期：{hz60:?}"
        );
        let hz144 = pace_interval_from(Some(144.0));
        assert!(
            (hz144.as_secs_f64() - 1.0 / 144.0).abs() < 1e-6,
            "144 Hz 提示 ⇒ 6.94 ms 周期：{hz144:?}"
        );
        assert_eq!(
            pace_interval_from(Some(0.5)),
            Duration::from_millis(MAX_PACE_WAIT_MS),
            "异常低的刷新率必须被上限保护（不能睡死）"
        );
    }
}

#[cfg(test)]
mod composition_health_tests {
    use std::cell::Cell;

    use super::composition_health_plan;
    use crate::{platform::windows::composition::CompositionState, window::WindowId};

    fn window(raw: u32) -> WindowId {
        WindowId::from_raw(raw)
    }

    #[test]
    fn the_composition_health_plan_consumes_a_new_epoch_exactly_once() {
        let id = window(7);
        let checks = Cell::new(0u32);

        let mut seen = 3;
        let plan = composition_health_plan(&mut seen, 3, std::iter::once(id), |_| {
            checks.set(checks.get() + 1);
            CompositionState::Invalid
        });
        assert!(plan.is_empty(), "代际相同不得产生重建清单");
        assert_eq!(checks.get(), 0, "代际相同不得逐窗检查");
        assert_eq!(seen, 3, "代际相同不得消费");

        let plan = composition_health_plan(&mut seen, 4, std::iter::once(id), |_| {
            checks.set(checks.get() + 1);
            CompositionState::Invalid
        });
        assert_eq!(plan, vec![id], "Invalid 窗口必须进重建清单");
        assert_eq!(checks.get(), 1, "代际变化必须逐窗检查一次");
        assert_eq!(seen, 4, "消费后必须记住新代际");

        let plan = composition_health_plan(&mut seen, 4, std::iter::once(id), |_| {
            checks.set(checks.get() + 1);
            CompositionState::Invalid
        });
        assert!(plan.is_empty());
        assert_eq!(checks.get(), 1, "同一代际只能消费一次");
    }

    #[test]
    fn only_invalid_windows_reach_the_rebuild_plan() {
        let mut seen = 0;
        let state_of = |id: WindowId| match id.to_bits() {
            1 => CompositionState::Invalid,
            2 => CompositionState::Valid,
            _ => CompositionState::WindowClosed,
        };
        let plan = composition_health_plan(&mut seen, 1, (1..=3).map(window), state_of);
        assert_eq!(
            plan,
            vec![window(1)],
            "只有 Invalid 窗口需要重建 visual tree"
        );
        assert_eq!(seen, 1);
    }

    #[test]
    fn the_pump_loop_wires_the_health_plan_into_rebuild_requests() {
        let source = include_str!("pump.rs");
        let plan_call = concat!("composition_health", "_plan(");
        let request = concat!("request_composition", "_rebuild()");
        assert!(
            source.matches(plan_call).count() >= 2,
            "pump.rs 必须同时包含 composition_health_plan 的定义与 pump_loop 内调用（删掉 \
             WM_PAINT 消费链即红）"
        );
        assert!(
            source.contains(request),
            "pump.rs 必须把 Invalid 清单落到 request_composition_rebuild（删掉请求即红）"
        );
    }
}

#[cfg(test)]
mod chrome_loop_wiring_tests {
    #[test]
    fn the_pump_consumes_the_control_wm_size_before_reconciling_chrome() {
        let source = include_str!("pump.rs");
        let control = source
            .find(concat!("native.apply_window", "_control(control);"))
            .expect("pump_loop 必须应用窗口控制请求");
        let after_control = &source[control..];
        let collect = after_control
            .find(concat!(
                "collect_events",
                "(&mut app, windows, dispatcher)?"
            ))
            .expect("控制执行后必须在同一轮补收 `WM_SIZE` 事件（P2-1 同帧闭环；删掉补收即红）");
        let sync = after_control
            .find(concat!("native.sync_chrome", "(&mut app)"))
            .expect("泵必须在事件消费之后逐窗口对账 chrome");
        assert!(
            collect < sync,
            "顺序必须是：应用控制 → 补收 `WM_SIZE` → `sync_chrome`（P2-1）；实际 collect={collect} \
             sync={sync}"
        );
    }
}

#[cfg(test)]
mod e10_deadline_timer_tests {
    use std::time::{Duration, Instant};

    use super::deadline_timer_millis;

    #[test]
    fn the_deadline_timer_rounds_up_and_never_arms_zero() {
        let now = Instant::now();
        assert_eq!(
            deadline_timer_millis(now + Duration::from_micros(500_400), now),
            501,
            "向上取整：500.4 ms 必须挂 501 ms（不能早于 deadline 唤醒）"
        );
        assert_eq!(
            deadline_timer_millis(now + Duration::from_millis(500), now),
            500
        );
        assert_eq!(
            deadline_timer_millis(now, now),
            1,
            "已到期 / 立即到期必须挂 1 ms（SetTimer(0) 是取消，会睡过到期点）"
        );
        assert_eq!(
            deadline_timer_millis(now.checked_sub(Duration::from_secs(1)).unwrap_or(now), now),
            1,
            "过去的 deadline 同样夹到 1 ms"
        );
        assert_eq!(
            deadline_timer_millis(now + Duration::from_secs(u64::from(u32::MAX)), now),
            u32::MAX,
            "超长延时夹到 u32::MAX，不得回绕"
        );
    }
}
