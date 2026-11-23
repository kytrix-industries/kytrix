use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use calloop::{EventLoop, ping::make_ping};

use crate::{
    app::App,
    error::{EngineError, WaylandErrorKind},
    math::{Scale, Size},
    platform::{
        dispatcher::Dispatcher,
        linux::{
            a11y::UnixA11y,
            clipboard::PendingSend,
            dpi,
            surface::{self, WaylandState},
            wake,
        },
        wayland::{close, drag},
    },
    render::backend::Renderer,
    window::{FrameOutcome, WindowControl, WindowId},
};

const FRAME_TICK: Duration = Duration::from_millis(16);

struct WindowA11y {
    id: WindowId,
    adapter: UnixA11y,
}

struct WindowMirror {
    id: WindowId,
    scale: Scale,
    size: Size,
    title: smol_str::SmolStr,
}

pub(crate) fn run(app: App) -> Result<(), EngineError> {
    let dispatcher = app.dispatcher();
    dispatcher.attach(1);

    let (first_id, first_options) = app
        .windows()
        .min_by_key(|(id, _)| id.to_bits())
        .map(|(id, window)| (id, window.options().clone()))
        .ok_or(EngineError::AppInit {
            detail: smol_str::SmolStr::new_static("Application::run without a window"),
        })?;
    let (mut state, mut queue) = WaylandState::new(first_id, &first_options)?;
    surface::initial_roundtrip(&mut queue, &mut state)?;
    surface::create_devices(&mut state)?;
    let mut app = app;

    let mut event_loop: EventLoop<'static, WaylandState> =
        EventLoop::try_new().map_err(|error| EngineError::AppInit {
            detail: smol_str::format_smolstr!("calloop: {error}"),
        })?;
    let (ping, ping_source) = make_ping().map_err(|error| EngineError::AppInit {
        detail: smol_str::format_smolstr!("calloop ping: {error}"),
    })?;
    event_loop
        .handle()
        .insert_source(ping_source, |_, _, _| {})
        .map_err(|error| EngineError::AppInit {
            detail: smol_str::format_smolstr!("calloop ping insert: {error}"),
        })?;
    wake::install(ping);
    let connection = state.connection().clone();
    calloop_wayland::WaylandSource::new(connection, queue)
        .insert(event_loop.handle())
        .map_err(|error| EngineError::AppInit {
            detail: smol_str::format_smolstr!("wayland source: {error}"),
        })?;
    let loop_handle = event_loop.handle();

    let mut a11ys: Vec<WindowA11y> = Vec::new();
    let mut mirrors: Vec<WindowMirror> = Vec::new();
    let result = (|| -> Result<(), EngineError> {
        sync_windows(&mut app, &mut state, &mut a11ys, &mut mirrors, &dispatcher)?;
        dispatcher.wake();
        let mut deferred_frames: Vec<WindowId> = Vec::new();
        loop {
            sync_windows(&mut app, &mut state, &mut a11ys, &mut mirrors, &dispatcher)?;
            if app.windows().next().is_none() {
                return Ok(());
            }
            state.emit_due_key_repeats(Instant::now());
            let events = state.take_events();
            let had_events = !events.is_empty();
            for (target, event) in events {
                if let Some(position) = drag::drag_press_position(&event)
                    && app
                        .window(target)
                        .is_some_and(|window| drag::hit(window.drag_regions(), position))
                    && let Some(index) = state.window_index(target)
                {
                    state.begin_move(index);
                    continue;
                }
                app.dispatch(target, event);
            }
            if had_events {
                dispatcher.wake();
            }
            app.poll_renderers()?;
            state.backfill_presentation(&mut app);
            for mirrored in &mut mirrors {
                let Some(window) = app.window_mut(mirrored.id) else {
                    continue;
                };
                let Some(index) = state.window_index(mirrored.id) else {
                    continue;
                };
                let scale_now = state.scale(index);
                if scale_now != mirrored.scale {
                    window.set_scale(scale_now);
                    mirrored.scale = scale_now;
                }
                let size_now = state.size(index);
                if size_now != mirrored.size {
                    window.set_size(size_now);
                    mirrored.size = size_now;
                }
                window.set_buffer_scale(state.buffer_scale(index));
                window.set_surface(dpi::physical_size(size_now, scale_now));
                if let Some(hz) = state.refresh_hz(index) {
                    window.set_frame_refresh_hz(hz);
                }
                if !state.is_visible(index) {
                    window.clear_frame_budget_deferred();
                }
                if window.set_state(state.window_state(index)) {
                    dispatcher.wake();
                }
                let title = window.options().title.clone();
                if title != mirrored.title {
                    state.set_title(index, &title);
                    mirrored.title = title;
                }
            }
            for window_a11y in &mut a11ys {
                let Some(index) = state.window_index(window_a11y.id) else {
                    continue;
                };
                window_a11y.adapter.focus_changed(state.is_focused(index));
            }
            if let Some((window, cursor)) = app.take_cursor_change() {
                state.set_cursor_shape(window, cursor);
            }
            let _ = app.take_capture_requests();
            state.degrade_text_input_if_silent(Instant::now());
            for pending in state.clipboard_mut().take_pending_sends() {
                let _ = register_pending_send(&loop_handle, pending);
            }

            let mut closing: Vec<WindowId> = Vec::new();
            for index in 0..state.window_count() {
                if state.take_close_request(index) {
                    closing.push(state.window_id(index));
                }
            }
            for id in close::handle_close_requests(&mut app, &closing) {
                state.remove_window(id);
                a11ys.retain(|entry| entry.id != id);
                mirrors.retain(|entry| entry.id != id);
            }
            if app.windows().next().is_none() {
                return Ok(());
            }

            app.sync_clock_live();
            app.run_frame_hook();
            if let Some((window, control)) = app.take_window_control()
                && let Some(index) = state.window_index(window)
            {
                match control {
                    WindowControl::Minimize => state.minimize(index),
                    WindowControl::ToggleMaximize => state.toggle_maximize(index),
                    WindowControl::ToggleFullscreen => state.toggle_fullscreen(index),
                    WindowControl::BeginMove => state.begin_move(index),
                    WindowControl::BeginResize(edge) => state.begin_resize(index, edge),
                }
            }
            if state.take_redraw() {
                dispatcher.wake();
            }
            let frame_windows: Vec<(usize, WindowId)> = (0..state.window_count())
                .map(|index| (index, state.window_id(index)))
                .filter(|(index, id)| {
                    state.is_visible(*index)
                        && app.window(*id).is_some_and(|window| window.has_renderer())
                })
                .collect();
            deferred_frames.retain(|id| state.window_index(*id).is_some());
            if frame_windows
                .iter()
                .any(|(index, _)| state.frame_ready(*index))
            {
                let wake = dispatcher.take_frame_wake();
                for (index, id) in frame_windows {
                    if !state.frame_ready(index) {
                        if wake && !deferred_frames.contains(&id) {
                            deferred_frames.push(id);
                        }
                        continue;
                    }
                    if !wake && !deferred_frames.contains(&id) {
                        continue;
                    }
                    deferred_frames.retain(|deferred| *deferred != id);
                    state.request_frame(index);
                    match app.frame(id) {
                        None | Some(Ok(FrameOutcome::Idle)) => {
                            state.note_frame_not_committed(index);
                            let budget_deferred = app
                                .window(id)
                                .is_some_and(crate::window::Window::frame_budget_deferred);
                            if !budget_deferred {
                                state.ack_pending_configure(index);
                            }
                            if budget_deferred && !deferred_frames.contains(&id) {
                                deferred_frames.push(id);
                            }
                        }
                        Some(Ok(FrameOutcome::Committed(_))) => {
                            state.ack_pending_configure(index);
                        }
                        Some(Err(error)) => return Err(error),
                    }
                }
            }
            for index in 0..state.window_count() {
                let id = state.window_id(index);
                let renders = state.is_visible(index)
                    && app.window(id).is_some_and(|window| window.has_renderer());
                if !renders {
                    state.ack_pending_configure(index);
                }
            }
            for task in dispatcher.drain_tasks() {
                task.run(&mut app);
            }
            state.sync_ime_policy(&app);

            let serial = state.serial();
            state.clipboard_mut().apply_pending(serial);
            for window_a11y in &mut a11ys {
                let Some(index) = state.window_index(window_a11y.id) else {
                    continue;
                };
                window_a11y.adapter.set_bounds(
                    crate::math::Point::ZERO,
                    state.size(index),
                    state.scale(index),
                );
                let id = window_a11y.id;
                if app.a11y_pending(id) {
                    window_a11y
                        .adapter
                        .update(|| crate::a11y::adapter::build_update(&mut app, id));
                }
            }
            app.wake_for_a11y_backlog(dispatcher.as_ref());

            if app.needs_continuous_frames() {
                dispatcher.wake();
            }
            let timeout = if app.needs_continuous_frames() {
                FRAME_TICK
            } else {
                surface::IDLE_TIMEOUT
            };
            let timeout = if app.a11y_retry_pending() {
                timeout.min(FRAME_TICK)
            } else {
                timeout
            };
            let timeout = match state.next_key_repeat_in(Instant::now()) {
                Some(due) => timeout.min(due),
                None => timeout,
            };
            let timeout = match state.next_ime_degrade_in(Instant::now()) {
                Some(due) => timeout.min(due),
                None => timeout,
            };
            let timeout = match app.next_wake_deadline() {
                Some(deadline) => timeout
                    .min(deadline.saturating_duration_since(Instant::now()))
                    .max(Duration::from_millis(1)),
                None => timeout,
            };
            if app.windows().next().is_none() {
                return Ok(());
            }
            event_loop
                .dispatch(timeout, &mut state)
                .map_err(|error| EngineError::Wayland {
                    kind: match &error {
                        calloop::Error::IoError(io) => WaylandErrorKind::from_io_error(io),
                        _ => WaylandErrorKind::Protocol,
                    },
                    detail: smol_str::format_smolstr!("calloop dispatch: {error}"),
                })?;
        }
    })();
    drop(app);
    result
}

fn register_pending_send(
    handle: &calloop::LoopHandle<'static, WaylandState>,
    pending: PendingSend,
) -> Result<(), calloop::Error> {
    let source =
        calloop::generic::Generic::new(pending, calloop::Interest::WRITE, calloop::Mode::Level);
    let inserted = handle.insert_source(source, |_, pending, _| match pending.write_more() {
        Ok(false) => Ok(calloop::PostAction::Continue),
        Ok(true) | Err(_) => Ok(calloop::PostAction::Remove),
    });
    match inserted {
        Ok(_token) => Ok(()),
        Err(error) => {
            drop(error.inserted);
            Err(error.error)
        }
    }
}

fn sync_windows(
    app: &mut App,
    state: &mut WaylandState,
    a11ys: &mut Vec<WindowA11y>,
    mirrors: &mut Vec<WindowMirror>,
    dispatcher: &Arc<Dispatcher>,
) -> Result<(), EngineError> {
    let stale: Vec<WindowId> = (0..state.window_count())
        .map(|index| state.window_id(index))
        .filter(|id| app.window(*id).is_none())
        .collect();
    for id in stale {
        state.remove_window(id);
        a11ys.retain(|entry| entry.id != id);
        mirrors.retain(|entry| entry.id != id);
    }
    let mut missing: Vec<(WindowId, crate::window::WindowOptions)> = app
        .windows()
        .filter(|(id, _)| state.window_index(*id).is_none())
        .map(|(id, window)| (id, window.options().clone()))
        .collect();
    missing.sort_by_key(|(id, _)| id.to_bits());
    for (id, options) in missing {
        state.add_window(id, &options)?;
    }
    for index in 0..state.window_count() {
        let id = state.window_id(index);
        if a11ys.iter().any(|entry| entry.id == id) {
            continue;
        }
        let Some(epoch) = app.window_a11y_epoch(id) else {
            continue;
        };
        a11ys.push(WindowA11y {
            id,
            adapter: UnixA11y::new(dispatcher.clone(), id, epoch)?,
        });
    }
    let renderable: Vec<(usize, WindowId)> = (0..state.window_count())
        .map(|index| (index, state.window_id(index)))
        .filter(|(index, id)| {
            state.is_configured(*index)
                && app.window(*id).is_some_and(|window| !window.has_renderer())
        })
        .collect();
    for (index, id) in renderable {
        let size = state.size(index);
        let scale = state.scale(index);
        let physical = dpi::physical_size(size, scale);
        let buffer_scale = state.buffer_scale(index);
        let (display, surface_ptr) = state.raw_pointers(index);
        let viewport = state.take_viewport(index);
        let surface = state.surface_clone(index);
        let presentation = state.presentation_handle(index);
        let Some(background) = app.window(id).map(|window| window.options().background) else {
            continue;
        };
        let renderer = Renderer::new_for_surface(
            display,
            surface_ptr,
            physical,
            background,
            viewport,
            surface,
            presentation,
        )?;
        let Some(window) = app.window_mut(id) else {
            continue;
        };
        window.set_renderer(renderer);
        window.set_scale(scale);
        window.set_size(size);
        window.set_buffer_scale(buffer_scale);
        window.set_surface(physical);
        window.set_state(state.window_state(index));
        mirrors.push(WindowMirror {
            id,
            scale,
            size,
            title: window.options().title.clone(),
        });
    }
    Ok(())
}

mod calloop_wayland;
