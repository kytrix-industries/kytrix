use std::{
    cell::RefCell,
    ffi::c_void,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
    NSWindow,
};
use objc2_core_foundation::{
    CFRunLoop, CFRunLoopActivity, CFRunLoopObserver, CFRunLoopObserverContext, CFRunLoopTimer,
};
use objc2_foundation::NSPoint;
use smallvec::SmallVec;

use crate::{
    app::App,
    error::EngineError,
    interaction::InputThresholds,
    platform::{
        dispatcher::Dispatcher,
        macos::{
            app_delegate::{AppDelegate, QuitHandshake},
            menu, wake,
            window::{NativeWindow, OwnerWindow},
        },
        macos_close::CloseAction,
        macos_owner::{
            ModalOwnerChromes, OwnerResolution, WindowKeyInput, reclaim_modal_keys, resolve_owner,
            window_level,
        },
        macos_pump,
    },
    render::{backend::Renderer, macos::core::GpuCore},
    window::{Window, WindowId, WindowOptions},
};

struct LoopState {
    app: *mut App,
    windows: Vec<NativeWindow>,
    dispatcher: Arc<Dispatcher>,
    modal_registry: Rc<RefCell<ModalOwnerChromes>>,
    ns_app: Retained<NSApplication>,
    _delegate: Retained<AppDelegate>,
    quit: Arc<QuitHandshake>,
    double_click: Duration,
    quitting: bool,
    exit: bool,
    error: Option<EngineError>,
}

impl Drop for LoopState {
    fn drop(&mut self) {
        unsafe { drop(Box::from_raw(self.app)) };
    }
}

fn double_click_time() -> Duration {
    let defaults = InputThresholds::platform_default();
    let interval = NSEvent::doubleClickInterval();
    if interval.is_finite() && interval > 0.0 {
        Duration::from_secs_f64(interval)
    } else {
        defaults.double_click_time
    }
}

fn install_input_thresholds(app: &mut App) {
    let defaults = InputThresholds::platform_default();
    app.set_input_thresholds(InputThresholds {
        double_click_time: double_click_time(),
        ..defaults
    });
}

fn sync_double_click_interval(app: &mut App, seen: &mut Duration) {
    let current = double_click_time();
    if current == *seen {
        return;
    }
    *seen = current;
    let defaults = InputThresholds::platform_default();
    app.set_input_thresholds(InputThresholds {
        double_click_time: current,
        ..defaults
    });
}

pub(crate) fn run(app: App) -> Result<(), EngineError> {
    let mtm = MainThreadMarker::new().ok_or(EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("NSApplication requires the main thread"),
    })?;
    let ns_app = NSApplication::sharedApplication(mtm);
    ns_app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    ns_app.finishLaunching();
    NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
    menu::install(mtm, &ns_app);

    let mut app = app;
    let dispatcher = app.dispatcher();
    let quit = QuitHandshake::new();
    let delegate = AppDelegate::new(mtm, Arc::clone(&dispatcher), Arc::clone(&quit));
    ns_app.setDelegate(Some(delegate.protocol()));
    dispatcher.attach(1);
    install_input_thresholds(&mut app);
    let requests: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    let mut windows: Vec<NativeWindow> = Vec::with_capacity(requests.len());
    let modal_registry: Rc<RefCell<ModalOwnerChromes>> =
        Rc::new(RefCell::new(ModalOwnerChromes::default()));
    create_native_windows(
        mtm,
        &app,
        &requests,
        &dispatcher,
        &mut windows,
        &modal_registry,
    )?;
    attach_owners(&requests, &mut windows, 0);
    for (native, (_, options)) in windows.iter_mut().zip(requests.iter()) {
        show_window(&mut app, native, options, &dispatcher)?;
    }
    ns_app.activate();

    let state = Box::into_raw(Box::new(LoopState {
        app: Box::into_raw(Box::new(app)),
        windows,
        dispatcher: dispatcher.clone(),
        modal_registry,
        ns_app: ns_app.clone(),
        _delegate: delegate,
        quit: Arc::clone(&quit),
        double_click: double_click_time(),
        quitting: false,
        exit: false,
        error: None,
    }));
    if !wake::install(state as *mut c_void, pump_callback, timer_callback) {
        unsafe { drop(Box::from_raw(state)) };
        return Err(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("CFRunLoop source installation failed"),
        });
    }
    let run_loop = CFRunLoop::main().ok_or(EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("main CFRunLoop is unavailable"),
    })?;
    let observer = {
        let mut context = CFRunLoopObserverContext {
            version: 0,
            info: state as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };
        unsafe {
            CFRunLoopObserver::new(
                None,
                CFRunLoopActivity::BeforeWaiting.0,
                true,
                0,
                Some(observer_callback),
                &mut context,
            )
        }
    };
    let Some(observer) = observer else {
        wake::uninstall();
        unsafe { drop(Box::from_raw(state)) };
        return Err(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("CFRunLoop observer installation failed"),
        });
    };
    run_loop.add_observer(Some(&observer), wake::common_mode());

    dispatcher.wake();
    ns_app.run();

    run_loop.remove_observer(Some(&observer), wake::common_mode());
    wake::uninstall();
    let mut state = unsafe { Box::from_raw(state) };
    let error = state.error.take();
    let awaiting_reply = state.quit.pending();
    let allowed = state.quit.allowed();
    drop(state);
    if awaiting_reply || allowed {
        ns_app.replyToApplicationShouldTerminate(allowed && error.is_none());
    }
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn create_native_windows(
    mtm: MainThreadMarker,
    app: &App,
    requests: &[(WindowId, WindowOptions)],
    dispatcher: &Arc<Dispatcher>,
    windows: &mut Vec<NativeWindow>,
    modal_registry: &Rc<RefCell<ModalOwnerChromes>>,
) -> Result<(), EngineError> {
    let start = windows.len();
    windows.reserve(requests.len());
    for (offset, (id, options)) in requests.iter().enumerate() {
        let a11y_epoch = app
            .window_a11y_epoch(*id)
            .ok_or_else(|| EngineError::AppInit {
                detail: smol_str::SmolStr::new_static(
                    "window disappeared while creating native window",
                ),
            })?;
        windows.push(NativeWindow::create(
            mtm,
            *id,
            options,
            dispatcher,
            start + offset,
            a11y_epoch,
            Rc::clone(modal_registry),
        )?);
    }
    Ok(())
}

fn attach_owners(
    requests: &[(WindowId, WindowOptions)],
    windows: &mut [NativeWindow],
    start: usize,
) {
    for (offset, (id, options)) in requests.iter().enumerate() {
        let index = start + offset;
        let resolution = resolve_owner(*id, options.owner, |owner| {
            windows.iter().any(|native| native.id() == owner)
        });
        let owner = match resolution {
            OwnerResolution::Attach(owner) => windows
                .iter()
                .find(|native| native.id() == owner)
                .map(|native| OwnerWindow {
                    id: owner,
                    window: native.window_handle(),
                }),
            OwnerResolution::None => None,
            OwnerResolution::Missing => {
                eprintln!(
                    "kytrix: 窗口 {id:?} 的 owner {:?} 没有原生窗口；降级为无 owner",
                    options.owner
                );
                None
            }
        };
        let level = window_level(options.always_on_top, options.modal && owner.is_some());
        windows[index].apply_owner(owner, level);
    }
}

fn show_window(
    app: &mut App,
    native: &mut NativeWindow,
    options: &WindowOptions,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let id = native.id();
    native.show();
    let scale = native.scale();
    let size = native.backing_size();
    let renderer = Renderer::new_for_layer(
        size,
        f64::from(scale.factor()),
        native.maximum_frames_per_second(),
        options.background,
        Arc::clone(dispatcher),
    )?;
    {
        let layer = renderer
            .layer_handle()
            .ok_or_else(|| EngineError::AppInit {
                detail: smol_str::SmolStr::new_static("metal renderer has no CAMetalLayer"),
            })?;
        native.attach_layer(layer);
    }
    if let Some(window) = app.window_mut(id) {
        window.set_renderer(renderer);
        window.set_scale(scale);
        window.set_size(native.logical_size());
        window.set_surface(size);
        window.set_frame_refresh_hz(native.maximum_frames_per_second());
    }
    Ok(())
}

fn pump_and_stop(state: &mut LoopState, tick: bool) {
    if state.exit {
        return;
    }
    if let Err(error) = pump(state, tick) {
        state.error = Some(error);
        state.exit = true;
    }
    if state.exit {
        wake::uninstall();
        if let Some(event) = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        ) {
            state.ns_app.postEvent_atStart(&event, true);
        }
        state.ns_app.stop(None);
        if let Some(run_loop) = CFRunLoop::current() {
            run_loop.stop();
            run_loop.wake_up();
        }
    }
}

fn pump(state: &mut LoopState, tick: bool) -> Result<(), EngineError> {
    let app = unsafe { &mut *state.app };
    app.sync_clock_live();
    sync_native_windows(
        app,
        &mut state.windows,
        &state.dispatcher,
        &state.modal_registry,
    )?;
    let recovered = poll_renderers_recovering(app, &state.windows, &state.dispatcher)?;
    sync_double_click_interval(app, &mut state.double_click);

    let mut changed = recovered;
    let key_inputs: SmallVec<[WindowKeyInput; 8]> =
        state.windows.iter().map(NativeWindow::key_input).collect();
    if let Some(modal) = reclaim_modal_keys(&key_inputs)
        && let Some(native) = state.windows.iter().find(|native| native.id() == modal)
    {
        native.make_key();
    }
    if let Some((window, control)) = app.take_window_control()
        && let Some(native) = state
            .windows
            .iter()
            .find(|native| native.id() == window && !native.is_destroyed())
    {
        native.apply_window_control(control);
        changed = true;
    }
    for native in state.windows.iter_mut() {
        let id = native.id();
        for event in native.take_events() {
            app.dispatch(id, event);
            changed = true;
        }
        changed |= native.sync_state(app)?;
        changed |= native.sync_chrome(app);
        if native.take_focus_lost() {
            native.handle_focus_lost(app);
        }
    }

    let gate = macos_pump::RoundInputs {
        tick,
        changed,
        pending_wake: state.dispatcher.has_pending_wake(),
        timer_due: app
            .next_wake_deadline()
            .is_some_and(|deadline| deadline <= Instant::now()),
        continuous: app.needs_continuous_frames(),
        deferred: app.has_deferred_frames(),
        a11y_retry: app.a11y_retry_pending(),
        a11y_backlog: app.a11y_backlog_pending(),
        quit: state.quit.pending(),
    };
    if !macos_pump::app_round(gate) {
        sync_display_pacing(app, &state.windows, gate.continuous);
        for native in state.windows.iter_mut() {
            native.update_a11y(app);
        }
        return Ok(());
    }

    app.run_frame_hook();
    let continuous = app.needs_continuous_frames();
    let deferred = app.has_deferred_frames();
    let wake = state.dispatcher.take_frame_wake();
    if wake || changed || (tick && (continuous || deferred)) {
        frame_all(app, &state.windows, tick && (continuous || deferred))?;
    }

    sync_display_pacing(app, &state.windows, continuous);

    if let Some((target, cursor)) = app.take_cursor_change()
        && let Some(native) = state
            .windows
            .iter()
            .find(|native| native.id() == target && !native.is_destroyed())
    {
        native.set_cursor(cursor);
    }
    let _ = app.take_capture_requests();

    for native in state.windows.iter_mut() {
        native.update_a11y(app);
        native.sync_ime_enabled(app);
        native.sync_ime_cursor_area(app);
        if let Some(window) = app.window(native.id()) {
            native.sync_drag_regions(window.drag_regions());
        }
    }
    app.wake_for_a11y_backlog(state.dispatcher.as_ref());
    for task in state.dispatcher.drain_tasks() {
        task.run(app);
    }

    if let Some(native) = state.windows.first() {
        native.set_animating(
            app.needs_continuous_frames() || app.has_deferred_frames() || app.a11y_retry_pending(),
        );
        native.set_wake_deadline(app.next_wake_deadline());
    }

    if state.quit.pending() && !state.quitting {
        state.quitting = true;
        request_quit(state, app);
        state.quitting = false;
    }

    let mut removed = false;
    for native in state.windows.iter_mut() {
        let id = native.id();
        let action = native.decide_close(|| {
            app.dispatch(id, crate::event::Event::CloseRequested);
            app.request_close(id)
        });
        match action {
            CloseAction::Keep => {}
            CloseAction::Close => {
                native.close();
                removed = true;
            }
            CloseAction::RemoveNative => {
                app.close_window(id);
                native.mark_externally_closed();
                removed = true;
            }
        }
    }
    if removed {
        state.windows.retain(|native| !native.is_destroyed());
    }
    if state.windows.is_empty() || app.windows().next().is_none() {
        state.exit = true;
    }
    Ok(())
}

fn sync_display_pacing(app: &mut App, windows: &[NativeWindow], continuous: bool) {
    for native in windows {
        let visible = native.is_visible();
        if let Some(window) = app.window_mut(native.id()) {
            window.set_frame_refresh_hz(native.maximum_frames_per_second());
            if !visible {
                window.clear_frame_budget_deferred();
            }
            window.sync_pacing(continuous && visible);
        }
    }
}

fn request_quit(state: &mut LoopState, app: &mut App) {
    let ids: Vec<WindowId> = app.windows().map(|(id, _)| id).collect();
    if !app.request_close_all(&ids) {
        state.quit.veto();
        state.ns_app.replyToApplicationShouldTerminate(false);
        return;
    }
    for native in state.windows.iter_mut() {
        native.close();
    }
    state.quit.allow();
}

fn sync_native_windows(
    app: &mut App,
    windows: &mut Vec<NativeWindow>,
    dispatcher: &Arc<Dispatcher>,
    modal_registry: &Rc<RefCell<ModalOwnerChromes>>,
) -> Result<(), EngineError> {
    windows.retain(|native| app.window(native.id()).is_some());
    let existing: Vec<WindowId> = windows.iter().map(NativeWindow::id).collect();
    let pending: Vec<(WindowId, WindowOptions)> = app
        .windows()
        .filter(|(id, _)| !existing.contains(id))
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let mtm = MainThreadMarker::new().ok_or(EngineError::AppInit {
        detail: smol_str::SmolStr::new_static("window creation requires the main thread"),
    })?;
    let start = windows.len();
    create_native_windows(mtm, app, &pending, dispatcher, windows, modal_registry)?;
    attach_owners(&pending, windows, start);
    for (native, (_, options)) in windows[start..].iter_mut().zip(pending.iter()) {
        show_window(app, native, options, dispatcher)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryDisposition {
    Rebuild,
    Fatal,
}

fn recovery_disposition(error: &EngineError) -> RecoveryDisposition {
    match error {
        EngineError::DeviceLost { .. } => RecoveryDisposition::Rebuild,
        _ => RecoveryDisposition::Fatal,
    }
}

fn recovery_invariant(detail: &'static str) -> EngineError {
    EngineError::AppInit {
        detail: smol_str::SmolStr::new_static(detail),
    }
}

fn poll_renderers_recovering(
    app: &mut App,
    windows: &[NativeWindow],
    dispatcher: &Arc<Dispatcher>,
) -> Result<bool, EngineError> {
    match app.poll_renderers() {
        Ok(()) => return Ok(false),
        Err(error) if matches!(recovery_disposition(&error), RecoveryDisposition::Rebuild) => {}
        Err(error) => return Err(error),
    }
    let mut recovered = false;
    for native in windows {
        let id = native.id();
        let error = match app.window_mut(id) {
            Some(window) => window.poll_renderer().err(),
            None => None,
        };
        let Some(error) = error else {
            continue;
        };
        match recovery_disposition(&error) {
            RecoveryDisposition::Fatal => return Err(error),
            RecoveryDisposition::Rebuild => {
                recover_window_renderer(app, native, dispatcher, &error)?;
                recovered = true;
            }
        }
    }
    Ok(recovered)
}

fn recover_window_renderer(
    app: &mut App,
    native: &NativeWindow,
    dispatcher: &Arc<Dispatcher>,
    error: &EngineError,
) -> Result<(), EngineError> {
    let started = Instant::now();
    let id = native.id();
    let EngineError::DeviceLost { reason, .. } = error else {
        return Err(recovery_invariant(
            "device recovery: unexpected non-device-lost error",
        ));
    };
    let size = native.backing_size();
    let native_dpr = native.scale();
    let scale = f64::from(native_dpr.factor());
    let max_frame_rate = native.maximum_frames_per_second();

    let (old, background) = {
        let window = app.window_mut(id).ok_or_else(|| {
            recovery_invariant("device recovery: the window disappeared before it was rebuilt")
        })?;
        let old = window.take_renderer().ok_or_else(|| {
            recovery_invariant("device recovery: the window has no renderer to rebuild")
        })?;
        let background = old.background_color().ok_or_else(|| {
            recovery_invariant("device recovery: the renderer has no window background")
        })?;
        (old, background)
    };
    let layer = old.layer_handle().ok_or_else(|| {
        recovery_invariant("device recovery: the window renderer has no CAMetalLayer")
    })?;
    let stale = old
        .gpu_core()
        .ok_or_else(|| recovery_invariant("device recovery: the window renderer has no GpuCore"))?;
    drop(old);
    let join_stall = started.elapsed();
    GpuCore::recover(&stale)?;
    let generation = GpuCore::generation();
    let renderer = Renderer::new_with_layer(
        layer,
        size,
        scale,
        max_frame_rate,
        background,
        Arc::clone(dispatcher),
    )?;
    let sink = renderer.frame_sink();
    let window = app.window_mut(id).ok_or_else(|| {
        recovery_invariant("device recovery: the window disappeared while it was rebuilt")
    })?;
    if window.has_compositor_ticket()
        && let Some(sink) = sink
    {
        crate::render::compositor_thread::compositor().register(id, sink);
    }
    window.set_renderer(renderer);
    window.set_surface(size);
    request_full_republish_after_recovery(window);

    eprintln!(
        "kytrix: metal device recovered for window {id:?} (GpuCore generation {generation}): \
         {reason}; main-thread stall {:.1} ms (old GPU thread join {:.1} ms); \
         visible gap ~1-3 frames",
        started.elapsed().as_secs_f64() * 1e3,
        join_stall.as_secs_f64() * 1e3
    );
    Ok(())
}

fn request_full_republish_after_recovery(window: &mut Window) {
    window.force_full_republish();
}

fn frame_all(app: &mut App, windows: &[NativeWindow], force: bool) -> Result<(), EngineError> {
    for native in windows {
        if !native.is_visible() {
            continue;
        }
        let outcome = if force {
            app.frame_forced(native.id())
        } else {
            app.frame(native.id())
        };
        match outcome {
            None | Some(Ok(_)) => {}
            Some(Err(error)) => return Err(error),
        }
    }
    Ok(())
}

unsafe extern "C-unwind" fn pump_callback(info: *mut c_void) {
    if info.is_null() {
        return;
    }
    let state = unsafe { &mut *(info as *mut LoopState) };
    pump_and_stop(state, false);
}

unsafe extern "C-unwind" fn observer_callback(
    _observer: *mut CFRunLoopObserver,
    _activity: CFRunLoopActivity,
    info: *mut c_void,
) {
    if info.is_null() {
        return;
    }
    let state = unsafe { &mut *(info as *mut LoopState) };
    pump_and_stop(state, false);
}

unsafe extern "C-unwind" fn timer_callback(timer: *mut CFRunLoopTimer, info: *mut c_void) {
    if info.is_null() {
        return;
    }
    wake::note_deadline_fired(timer);
    let state = unsafe { &mut *(info as *mut LoopState) };
    pump_and_stop(state, true);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use smol_str::SmolStr;

    use super::{RecoveryDisposition, recovery_disposition, request_full_republish_after_recovery};
    use crate::{error::EngineError, window::WindowOptions};

    #[test]
    fn only_device_lost_triggers_a_renderer_rebuild() {
        let lost = EngineError::DeviceLost {
            reason: SmolStr::new_static("MTLCommandBufferError(11) DeviceRemoved"),
            log_path: PathBuf::from("/tmp/kytrix-metal-diagnostics.txt"),
        };
        assert_eq!(
            recovery_disposition(&lost),
            RecoveryDisposition::Rebuild,
            "设备丢失必须走整 renderer 重建"
        );
        for fatal in [
            EngineError::Swapchain {
                detail: SmolStr::new_static("MTLCommandBuffer failed"),
            },
            EngineError::AppInit {
                detail: SmolStr::new_static("metal newTexture: returned nil"),
            },
            EngineError::FramePipeline {
                detail: SmolStr::new_static("compositor inbox closed"),
            },
            EngineError::UnsupportedGpu {
                os: "macos",
                required: "D8",
                actual: SmolStr::new_static("no Metal device"),
            },
        ] {
            assert_eq!(
                recovery_disposition(&fatal),
                RecoveryDisposition::Fatal,
                "{fatal:?} 不是设备丢失，不得触发整 renderer 重建（保持致命 + 应用退出）"
            );
        }
    }

    #[cfg(feature = "test-support")]
    #[test]
    fn recovery_forces_a_full_republish_of_the_next_frame() {
        use crate::{
            app::App,
            math::{LogicalRect, LogicalSize, px},
            node::builder::NodeBuilder,
            paint::{color::Color, style::PaintSpec},
            window::FrameOutcome,
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let mut app = App::new(runtime.handle().clone());
        let window = app.create_window(WindowOptions {
            title: SmolStr::new_static("device-recovery"),
            size: LogicalSize::new(px(320.0), px(240.0)),
            ..WindowOptions::default()
        });
        let root = app.window(window).expect("window").tree().root();
        let mut nodes = Vec::with_capacity(3);
        for index in 0..3 {
            let node = NodeBuilder::boxed()
                .layout_override(LogicalRect::new(
                    px(0.0),
                    px(index as f32 * 24.0),
                    px(32.0),
                    px(20.0),
                ))
                .paint(PaintSpec {
                    background: Color::WHITE,
                    ..PaintSpec::default()
                })
                .build(app.window_mut(window).expect("window").tree_mut(), root)
                .expect("box");
            nodes.push(node);
        }
        let _ = app
            .frame(window)
            .expect("window")
            .expect("first full frame");
        let _ = app
            .window_mut(window)
            .expect("window")
            .tree_mut()
            .mark_paint(nodes[0]);
        let _ = app
            .frame(window)
            .expect("window")
            .expect("incremental frame");
        let view = app.window(window).expect("window");
        assert_eq!(
            view.record().expect("record").stats.encoded_lanes,
            1,
            "前置条件：稳态帧必须是增量帧"
        );

        request_full_republish_after_recovery(app.window_mut(window).expect("window"));
        let outcome = app
            .frame(window)
            .expect("window")
            .expect("recovery republish frame");
        assert!(
            matches!(outcome, FrameOutcome::Committed(_)),
            "恢复后的整帧重发必须真的提交"
        );
        let record = app
            .window(window)
            .expect("window")
            .record()
            .expect("record");
        assert!(
            record.damage_full,
            "恢复后第一帧必须是 Plan::Full（新 persist 是空的）"
        );
        assert_eq!(
            record.stats.encoded_lanes, 3,
            "恢复后必须重编码全部 lane（新 GPU 侧实例镜像是空的）"
        );
    }
}
