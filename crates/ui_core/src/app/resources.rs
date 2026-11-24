use std::sync::Arc;

use bytes::Bytes;

use super::{App, AppHandle};
use crate::{
    error::EngineError,
    event::Event,
    math::{LogicalPx, LogicalRect, LogicalSize},
    node::image::{ImageFit, variant_target},
    platform::dispatcher::Dispatcher,
    resources::{
        DecodeState, ImageDiagnostic, PendingResource, ResourceCache, ResourceImage, VariantState,
    },
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
        let scale = window.scale();
        count += window.tree_mut().resolve_pending_images(&resolved, scale);
    }
    count
}

pub(crate) fn refresh_images(app: &mut App, window: WindowId) -> usize {
    let App {
        windows,
        resources,
        image_dpi_epochs,
        ..
    } = app;
    let Some(target) = windows.get_mut(&window) else {
        return 0;
    };
    let dpi_epoch = target.dpi_epoch();
    let scale_changed = image_dpi_epochs.get(&window).copied() != Some(dpi_epoch);
    let marked = target.tree_mut().take_images_need_refresh();
    if !marked && !scale_changed {
        return 0;
    }
    let dpr = target.scale();
    let updated = target.tree_mut().refresh_image_intrinsics(resources, dpr);
    if scale_changed {
        target.tree_mut().mark_bitmap_images_paint(resources);
    }
    image_dpi_epochs.insert(window, dpi_epoch);
    updated
}

pub(crate) fn prepare_image_variants(app: &mut App, window: WindowId) -> usize {
    let report = {
        let App {
            windows, resources, ..
        } = app;
        let Some(target) = windows.get_mut(&window) else {
            return 0;
        };
        let dpr = target.scale();
        resources.begin_variant_frame();
        target.tree_mut().prepare_image_variants(resources, dpr)
    };
    let created = report.created;
    for (resource, intrinsic, cause) in report.failures {
        app.record_image_diagnostic(ImageDiagnostic::VariantUnavailable {
            resource,
            width: intrinsic.width.value().max(0.0).round() as u32,
            height: intrinsic.height.value().max(0.0).round() as u32,
            cause,
        });
    }
    for resource in report.recovered {
        if !app.has_image_diagnostic(resource) {
            continue;
        }
        app.clear_image_diagnostic(resource);
        for candidate in app.windows.values_mut() {
            candidate
                .tree_mut()
                .invalidate_images_with_resource(resource);
        }
    }
    created
}

pub(crate) fn prepare_image_variant(
    app: &mut App,
    window: WindowId,
    image: ResourceImage,
    target: LogicalSize,
    fit: ImageFit,
) -> Option<crate::paint::atlas::AtlasTile> {
    let dpr = app.windows.get(&window)?.scale();
    let live = app.resources.image(image.id())?;
    let intrinsic = live.intrinsic(dpr);
    let bounds = LogicalRect::new(
        LogicalPx::ZERO,
        LogicalPx::ZERO,
        target.width,
        target.height,
    );
    let physical = variant_target(bounds, fit, intrinsic, dpr)?;
    match app.resources.ensure_variant(live, physical) {
        Some(VariantState::Ready { .. }) | Some(VariantState::Failed { .. }) => {
            let tile = app.resources.variant_for(live, physical);
            if tile.is_some() {
                app.resources.protect_variant(live, physical);
            }
            tile
        }
        None => None,
    }
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

pub(crate) fn insert_bitmap_for_window(
    app: &mut App,
    window: WindowId,
    bytes: &[u8],
) -> Result<ResourceImage, EngineError> {
    match app.windows.get(&window) {
        Some(_) => app.resources.insert_owned(window, bytes),
        None => Err(EngineError::UnknownWindow { window }),
    }
}

pub(crate) fn insert_svg_for_window(
    app: &mut App,
    window: WindowId,
    bytes: &[u8],
    intrinsic: LogicalSize,
) -> Result<ResourceImage, EngineError> {
    match app.windows.get(&window) {
        Some(_) => app.resources.insert_owned_svg(window, bytes, intrinsic),
        None => Err(EngineError::UnknownWindow { window }),
    }
}

pub(crate) fn insert_bitmap_async_for_window(
    app: &mut App,
    window: WindowId,
    bytes: Bytes,
) -> Result<PendingResource, EngineError> {
    match app.windows.get(&window) {
        Some(_) => Ok(app.resources.insert_owned_async(window, bytes)),
        None => Err(EngineError::UnknownWindow { window }),
    }
}

pub(crate) fn insert_svg_async_for_window(
    app: &mut App,
    window: WindowId,
    bytes: Bytes,
    intrinsic: LogicalSize,
) -> Result<PendingResource, EngineError> {
    match app.windows.get(&window) {
        Some(_) => app
            .resources
            .insert_owned_svg_async(window, bytes, intrinsic),
        None => Err(EngineError::UnknownWindow { window }),
    }
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
            let (_, consumed) = app.dispatch_input_tracked(window, input.clone());
            if consumed {
                return;
            }
        }
        Event::FocusChanged { focused } => {
            if !focused {
                app.on_window_focus_lost(window);
            }
            app.note_window_focus(window, *focused);
        }
        _ => {}
    }
    app.events.push(window, event);
}

#[cfg(test)]
thread_local! {
    static TEST_CLIPBOARD: std::cell::RefCell<Option<smol_str::SmolStr>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn set_clipboard_text(_app: &App, text: &str) -> Result<(), EngineError> {
    #[cfg(test)]
    {
        let _ = crate::platform::native::clipboard::set_text as fn(&str) -> Result<(), EngineError>;
        TEST_CLIPBOARD.with(|cell| {
            *cell.borrow_mut() = Some(smol_str::SmolStr::new(text));
        });
        Ok(())
    }
    #[cfg(not(test))]
    {
        crate::platform::native::clipboard::set_text(text)
    }
}

pub(crate) fn clipboard_text(_app: &App) -> Result<smol_str::SmolStr, EngineError> {
    #[cfg(test)]
    {
        let _ = crate::platform::native::clipboard::get_text
            as fn() -> Result<smol_str::SmolStr, EngineError>;
        Ok(TEST_CLIPBOARD.with(|cell| {
            cell.borrow()
                .clone()
                .unwrap_or_else(smol_str::SmolStr::default)
        }))
    }
    #[cfg(not(test))]
    {
        crate::platform::native::clipboard::get_text()
    }
}

#[must_use]
pub(crate) fn resources(app: &App) -> &ResourceCache {
    &app.resources
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
