use std::sync::Arc;

use super::{App, AppHandle};
use crate::{
    error::EngineError,
    event::Event,
    platform::dispatcher::Dispatcher,
    resources::{DecodeState, PendingResource, ResourceCache, ResourceImage},
    window::WindowId,
};

pub(crate) fn on_frame(app: &mut App, hook: impl FnMut(&mut App) + 'static) {
    app.frame_hook = Some(Box::new(hook));
}

pub(crate) fn run_frame_hook(app: &mut App) {
    poll_resources(app);
    if let Some(mut hook) = app.frame_hook.take() {
        hook(app);
        app.frame_hook = Some(hook);
    }
}

pub(crate) fn poll_resources(app: &mut App) {
    let results = app.resources.poll();
    if results.is_empty() {
        return;
    }
    apply_resource_results(app, &results);
    app.pending_resource_results.extend(results);
    app.request_frame();
}

pub(crate) fn apply_resource_results(
    app: &mut App,
    results: &[(PendingResource, DecodeState)],
) -> usize {
    let mut resolved: ahash::HashMap<PendingResource, ResourceImage> = ahash::HashMap::default();
    for (pending, state) in results {
        if let DecodeState::Ready(image) = state {
            resolved.insert(*pending, *image);
        }
    }
    if resolved.is_empty() {
        return 0;
    }
    let App { windows, .. } = app;
    let mut count = 0;
    for window in windows.values_mut() {
        let scale = window.scale().factor();
        count += window.tree_mut().resolve_pending_images(&resolved, scale);
    }
    count
}

pub(crate) fn refresh_images(app: &mut App, window: WindowId) -> usize {
    let App {
        windows,
        resources,
        image_scale_epochs,
        ..
    } = app;
    let Some(target) = windows.get_mut(&window) else {
        return 0;
    };
    let scale_epoch = target.scale_epoch();
    let scale_changed = image_scale_epochs.get(&window).copied() != Some(scale_epoch);
    let marked = target.tree_mut().take_images_need_refresh();
    if !marked && !scale_changed {
        return 0;
    }
    image_scale_epochs.insert(window, scale_epoch);
    let scale = target.scale().factor();
    target.tree_mut().refresh_image_intrinsics(resources, scale)
}

pub(crate) fn unload_resource(app: &mut App, id: crate::resources::ResourceId) -> bool {
    if !app.resources.unload(id) {
        return false;
    }
    let App { windows, .. } = app;
    let mut invalidated = 0;
    for window in windows.values_mut() {
        invalidated += window.tree_mut().invalidate_images_with_resource(id);
    }
    if invalidated > 0 {
        app.request_frame();
    }
    true
}

pub(crate) fn invalidate_dead_images(app: &mut App) -> usize {
    let App {
        windows, resources, ..
    } = app;
    let mut invalidated = 0;
    for window in windows.values_mut() {
        invalidated += window.tree_mut().invalidate_dead_images(resources);
    }
    invalidated
}

pub(crate) fn take_resource_results(app: &mut App) -> Vec<(PendingResource, DecodeState)> {
    core::mem::take(&mut app.pending_resource_results)
}

#[must_use]
pub(crate) fn handle(app: &App) -> AppHandle {
    AppHandle {
        dispatcher: app.dispatcher.clone(),
        runtime: app.runtime.clone(),
    }
}

pub(crate) fn dispatcher(app: &App) -> Arc<Dispatcher> {
    app.dispatcher.clone()
}

#[must_use]
pub(crate) fn drain_events(app: &mut App) -> Vec<(WindowId, Event)> {
    app.events.drain()
}

#[must_use]
pub(crate) fn drain_events_where(
    app: &mut App,
    keep: impl FnMut(WindowId, &Event) -> bool,
) -> Vec<(WindowId, Event)> {
    app.events.drain_where(keep)
}

pub(crate) fn dispatch(app: &mut App, window: WindowId, event: Event) {
    if matches!(event, Event::Input(_) | Event::Ime(_))
        && super::window_set::is_input_blocked_by_modal(app, window)
    {
        return;
    }
    match &event {
        Event::Input(input) => {
            app.sync_clock();
            app.dispatch_input(window, input.clone());
        }
        Event::FocusChanged { focused } => {
            if !focused {
                app.on_window_focus_lost(window);
            }
            app.note_window_focus(window, *focused);
        }
        Event::DpiChanged { scale, .. } => app.note_window_dpi(window, *scale),
        _ => {}
    }
    app.events.push(window, event);
}

pub(crate) fn set_clipboard_text(_app: &App, text: &str) -> Result<(), EngineError> {
    crate::platform::native::clipboard::set_text(text)
}

pub(crate) fn clipboard_text(_app: &App) -> Result<smol_str::SmolStr, EngineError> {
    crate::platform::native::clipboard::get_text()
}

#[must_use]
pub(crate) fn resources(app: &App) -> &ResourceCache {
    &app.resources
}

pub(crate) fn resources_mut(app: &mut App) -> &mut ResourceCache {
    &mut app.resources
}

#[must_use]
pub(crate) fn geometry(app: &App) -> &crate::paint::geometry_store::GeometryStore {
    &app.geometry
}

pub(crate) fn geometry_mut(app: &mut App) -> &mut crate::paint::geometry_store::GeometryStore {
    &mut app.geometry
}

#[must_use]
pub(crate) fn runtime_handle(app: &App) -> &tokio::runtime::Handle {
    &app.runtime
}

pub(crate) fn poll_renderers(app: &mut App) -> Result<(), EngineError> {
    for window in app.windows.values_mut() {
        window.poll_renderer()?;
    }
    Ok(())
}
