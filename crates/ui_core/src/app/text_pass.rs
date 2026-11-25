use std::rc::Rc;

use smallvec::SmallVec;

use super::App;
use crate::{
    error::EngineError,
    math::{LogicalPx, LogicalSize},
    text::{FontStack, FontStackStatus, TextDiagnostic, TextLayoutReport, TextPassReport},
    window::WindowId,
};

pub(crate) fn upload_atlas_pages(app: &mut App) {
    app.upload_glyph_page();
    app.upload_image_page();
}

pub(crate) fn upload_glyph_page(app: &mut App) {
    use crate::{paint::atlas::AtlasKind, render::AtlasArraySpec};

    let page = app.atlas.page();
    debug_assert_eq!(page.0, page.1, "glyph atlas page must stay square");
    let extent = page.0;
    let len = extent as usize * extent as usize;
    let spec = AtlasArraySpec {
        page: extent,
        layers: 1,
    };

    let cleared = app.glyph_atlas.take_cleared();
    if cleared {
        app.pending_dirty.clear();
        app.glyph_page_extent = None;
        for window in app.windows.values_mut() {
            window.clear_atlas_rects(AtlasKind::Glyph, 0);
            window.mark_atlas_page_stale(AtlasKind::Glyph);
        }
    }
    let page_changed = app.glyph_page_extent != Some(extent);
    if page_changed {
        app.pending_dirty.clear();
        app.glyph_page.clear();
        app.glyph_page.resize(len, 0);
        app.glyph_page_extent = Some(extent);
    }
    let refresh = cleared || page_changed;

    let dirty = std::mem::take(&mut app.pending_dirty);
    let has_rects = !dirty.is_empty();
    let any_missing = app
        .windows
        .values()
        .any(|window| window.has_renderer() && !window.atlas_page_matches(AtlasKind::Glyph, spec));
    let any_stale_idle = app.windows.values().any(|window| {
        window.has_renderer()
            && !window.atlas_page_matches(AtlasKind::Glyph, spec)
            && window.atlas_idle(AtlasKind::Glyph)
    });
    let any_queued = app
        .windows
        .values()
        .any(|window| window.has_renderer() && !window.atlas_rects(AtlasKind::Glyph, 0).is_empty());
    if !has_rects && !any_missing && !any_queued {
        return;
    }
    if refresh || any_stale_idle {
        app.glyph_atlas.write_page(&mut app.glyph_page, extent);
    } else if has_rects {
        app.glyph_atlas
            .write_rows(&mut app.glyph_page, extent, &dirty);
    }
    let full_rects = crate::render::DirtyRects::full();

    for window in app.windows.values_mut() {
        if !window.has_renderer() {
            continue;
        }
        if !window.atlas_page_matches(AtlasKind::Glyph, spec) {
            if !window.atlas_idle(AtlasKind::Glyph) {
                continue;
            }
            let uploaded = window.queue_atlas_upload(
                AtlasKind::Glyph,
                0,
                spec,
                &app.glyph_page,
                full_rects.clone(),
            );
            if uploaded {
                window.mark_atlas_page_received(AtlasKind::Glyph, spec);
                window.clear_atlas_rects(AtlasKind::Glyph, 0);
            }
            continue;
        }
        if has_rects {
            window
                .atlas_rects_mut(AtlasKind::Glyph, 0)
                .merge_from(&dirty);
        }
        if window.atlas_rects(AtlasKind::Glyph, 0).is_empty()
            || !window.atlas_idle(AtlasKind::Glyph)
        {
            continue;
        }
        if window.atlas_rects(AtlasKind::Glyph, 0).is_full() {
            if window.queue_atlas_upload(
                AtlasKind::Glyph,
                0,
                spec,
                &app.glyph_page,
                full_rects.clone(),
            ) {
                window.clear_atlas_rects(AtlasKind::Glyph, 0);
            }
            continue;
        }
        let mut packed = std::mem::take(&mut app.packed);
        crate::render::atlas::pack_rects(
            &app.glyph_page,
            extent,
            1,
            window.atlas_rects(AtlasKind::Glyph, 0),
            &mut packed,
        );
        let rects = window.atlas_rects(AtlasKind::Glyph, 0).clone();
        if window.queue_atlas_upload(AtlasKind::Glyph, 0, spec, &packed, rects) {
            window.clear_atlas_rects(AtlasKind::Glyph, 0);
        }
        app.packed = packed;
    }
}

const IMAGE_GROWTH_DEFER_LIMIT: u32 = 8;

pub(crate) fn upload_image_page(app: &mut App) {
    use crate::{
        paint::atlas::{AtlasKind, MAX_IMAGE_PAGES},
        render::AtlasArraySpec,
    };

    let page_count = app.resources.page_count();
    if page_count == 0 {
        for window in app.windows.values_mut() {
            if window.has_renderer() {
                window.set_image_drawable_layers(0);
            }
        }
        return;
    }
    let extent = app.resources.page_extent();
    debug_assert!(extent > 0, "image atlas page must be non-empty");
    debug_assert!(
        page_count <= usize::from(MAX_IMAGE_PAGES),
        "image atlas pages must respect MAX_IMAGE_PAGES"
    );
    let layers = u16::try_from(page_count).expect("image atlas page count fits u16");
    let target = AtlasArraySpec {
        page: extent,
        layers: u32::from(layers),
    };

    let dirty: Vec<crate::render::DirtyRects> = (0..layers)
        .map(|layer| app.resources.take_dirty_rects_at(layer).unwrap_or_default())
        .collect();
    let has_rects = dirty.iter().any(|rects| !rects.is_empty());
    let full_rects = crate::render::DirtyRects::full();

    let established = app.image_array_established;
    let bootstrap = established.is_none();
    let needs_growth =
        established.is_none_or(|spec| spec.page != target.page || spec.layers < target.layers);
    if needs_growth {
        let blocked: Vec<crate::window::WindowId> = app
            .windows
            .iter()
            .filter(|(_, window)| {
                window.has_renderer()
                    && (bootstrap || window.atlas_participating())
                    && !window.atlas_idle(AtlasKind::Image)
            })
            .map(|(id, _)| *id)
            .collect();
        if !blocked.is_empty() {
            app.image_growth_deferrals = app.image_growth_deferrals.saturating_add(1);
            let degraded = app.image_growth_deferrals >= IMAGE_GROWTH_DEFER_LIMIT;
            if !degraded {
                for window in app.windows.values_mut() {
                    if !window.has_renderer() {
                        continue;
                    }
                    if has_rects {
                        for layer in 0..layers {
                            window
                                .atlas_rects_mut(AtlasKind::Image, layer)
                                .merge_from(&dirty[usize::from(layer)]);
                        }
                    }
                    let drawable = window.image_array_spec().map_or(0, |spec| spec.layers);
                    window.set_image_drawable_layers(drawable);
                }
                for id in blocked {
                    if let Some(window) = app.windows.get_mut(&id) {
                        window.request_repaint();
                    }
                }
                app.dispatcher.wake();
                return;
            }
            for window in app.windows.values_mut() {
                if (bootstrap || window.atlas_participating())
                    && !window.atlas_idle(AtlasKind::Image)
                {
                    window.mark_atlas_page_stale(AtlasKind::Image);
                    window.set_image_drawable_layers(0);
                }
            }
        }
        let mut advanced = false;
        {
            let App {
                resources, windows, ..
            } = app;
            for window in windows.values_mut() {
                if !window.has_renderer() {
                    continue;
                }
                if (!bootstrap && !window.atlas_participating())
                    || !window.atlas_idle(AtlasKind::Image)
                {
                    let drawable = window.image_array_spec().map_or(0, |spec| spec.layers);
                    window.set_image_drawable_layers(drawable);
                    continue;
                }
                let mut enqueued = Vec::with_capacity(page_count);
                for layer in 0..layers {
                    let Some((dims, pixels)) = resources.page_pixels_at(layer) else {
                        break;
                    };
                    debug_assert_eq!(
                        (dims.0, dims.1),
                        (extent, extent),
                        "image atlas page must be square and uniform"
                    );
                    if !window.queue_atlas_upload(
                        AtlasKind::Image,
                        layer,
                        target,
                        pixels,
                        full_rects.clone(),
                    ) {
                        break;
                    }
                    enqueued.push(layer);
                }
                if enqueued.len() == page_count {
                    for layer in enqueued {
                        window.mark_image_layer_received(layer);
                    }
                    window.mark_atlas_page_received(AtlasKind::Image, target);
                    window.clear_all_atlas_rects(AtlasKind::Image);
                    window.set_image_drawable_layers(target.layers);
                    advanced = true;
                } else {
                    window.set_image_drawable_layers(0);
                }
            }
        }
        if advanced {
            app.image_array_established = Some(target);
            app.image_growth_deferrals = 0;
        }
        return;
    }

    {
        let App {
            resources,
            windows,
            packed,
            ..
        } = app;
        for window in windows.values_mut() {
            if !window.has_renderer() {
                continue;
            }
            if !window.atlas_participating() {
                if has_rects {
                    for layer in 0..layers {
                        window
                            .atlas_rects_mut(AtlasKind::Image, layer)
                            .merge_from(&dirty[usize::from(layer)]);
                    }
                }
                let drawable = window.image_array_spec().map_or(0, |spec| spec.layers);
                window.set_image_drawable_layers(drawable);
                continue;
            }
            let received = window.image_array_spec();
            let array_missing = received != Some(target);
            let missing_layers =
                array_missing || (0..layers).any(|layer| !window.image_layer_received(layer));
            if missing_layers {
                if has_rects {
                    for layer in 0..layers {
                        window
                            .atlas_rects_mut(AtlasKind::Image, layer)
                            .merge_from(&dirty[usize::from(layer)]);
                    }
                }
                if !window.atlas_idle(AtlasKind::Image) {
                    window.set_image_drawable_layers(received.map_or(0, |spec| spec.layers));
                    continue;
                }
                let mut enqueued = Vec::with_capacity(page_count);
                for layer in 0..layers {
                    if !array_missing && window.image_layer_received(layer) {
                        continue;
                    }
                    let Some((dims, pixels)) = resources.page_pixels_at(layer) else {
                        break;
                    };
                    debug_assert_eq!(
                        (dims.0, dims.1),
                        (extent, extent),
                        "image atlas page must be square and uniform"
                    );
                    if !window.queue_atlas_upload(
                        AtlasKind::Image,
                        layer,
                        target,
                        pixels,
                        full_rects.clone(),
                    ) {
                        break;
                    }
                    enqueued.push(layer);
                }
                let needed: usize = if array_missing {
                    page_count
                } else {
                    (0..layers)
                        .filter(|layer| !window.image_layer_received(*layer))
                        .count()
                };
                if enqueued.len() == needed {
                    for layer in enqueued {
                        window.mark_image_layer_received(layer);
                        window.clear_atlas_rects(AtlasKind::Image, layer);
                    }
                    window.mark_atlas_page_received(AtlasKind::Image, target);
                    if array_missing {
                        window.clear_all_atlas_rects(AtlasKind::Image);
                    }
                    window.set_image_drawable_layers(target.layers);
                } else {
                    window.set_image_drawable_layers(received.map_or(0, |spec| spec.layers));
                }
                continue;
            }
            window.set_image_drawable_layers(target.layers);
            let idle = window.atlas_idle(AtlasKind::Image);
            if !idle {
                if has_rects {
                    for layer in 0..layers {
                        window
                            .atlas_rects_mut(AtlasKind::Image, layer)
                            .merge_from(&dirty[usize::from(layer)]);
                    }
                }
                continue;
            }
            for layer in 0..layers {
                if has_rects {
                    window
                        .atlas_rects_mut(AtlasKind::Image, layer)
                        .merge_from(&dirty[usize::from(layer)]);
                }
                if window.atlas_rects(AtlasKind::Image, layer).is_empty() {
                    continue;
                }
                let Some((dims, pixels)) = resources.page_pixels_at(layer) else {
                    window.clear_atlas_rects(AtlasKind::Image, layer);
                    continue;
                };
                debug_assert_eq!((dims.0, dims.1), (extent, extent));
                if window.atlas_rects(AtlasKind::Image, layer).is_full() {
                    if window.queue_atlas_upload(
                        AtlasKind::Image,
                        layer,
                        target,
                        pixels,
                        full_rects.clone(),
                    ) {
                        window.clear_atlas_rects(AtlasKind::Image, layer);
                    }
                    continue;
                }
                let mut buf = std::mem::take(packed);
                crate::render::atlas::pack_rects(
                    pixels,
                    extent,
                    4,
                    window.atlas_rects(AtlasKind::Image, layer),
                    &mut buf,
                );
                let rects = window.atlas_rects(AtlasKind::Image, layer).clone();
                if window.queue_atlas_upload(AtlasKind::Image, layer, target, &buf, rects) {
                    window.clear_atlas_rects(AtlasKind::Image, layer);
                }
                *packed = buf;
            }
        }
    }
}

#[cfg(feature = "test-support")]
pub(crate) fn glyph_page_pixels(app: &App) -> ((u32, u32), Vec<u8>) {
    let page = app.atlas.page();
    let mut pixels = vec![0u8; (page.0 as usize) * (page.1 as usize)];
    app.glyph_atlas.write_page(&mut pixels, page.0);
    (page, pixels)
}

#[cfg(feature = "test-support")]
pub(crate) fn resource_page_pixels_all(app: &App) -> Vec<((u32, u32), &[u8])> {
    (0..app.resources.page_count() as u16)
        .filter_map(|page| app.resources.page_pixels_at(page))
        .collect()
}

pub(crate) fn refresh_text(
    app: &mut App,
    window: WindowId,
) -> Option<Result<TextPassReport, EngineError>> {
    let App {
        text,
        raster,
        text_dirty_scratch,
        windows,
        dpi_epochs,
        pending_dirty,
        ..
    } = app;
    let window_id = window;
    let dpi_epoch = windows.get(&window_id)?.dpi_epoch();
    if dpi_epochs.get(&window_id).copied().unwrap_or(0) != dpi_epoch {
        dpi_epochs.insert(window_id, dpi_epoch);
        if let Some(target) = windows.get_mut(&window_id) {
            target.tree_mut().invalidate_text_lines();
        }
    }
    raster.set_font_epoch(text.font_epoch());

    let window = windows.get_mut(&window_id)?;
    let tree = window.tree_mut();
    let mut report = TextPassReport::default();
    let known = taffy::Size {
        width: None,
        height: None,
    };
    let available = taffy::Size {
        width: taffy::AvailableSpace::MaxContent,
        height: taffy::AvailableSpace::MaxContent,
    };

    let mut index = 0usize;
    tree.take_text_dirty(text_dirty_scratch);
    let dirty: &[crate::node::id::NodeId] = text_dirty_scratch;
    while index < dirty.len() {
        let node = dirty[index];
        index += 1;
        let Some(content) = tree.text_content(node).cloned() else {
            continue;
        };
        let editable = tree.editable(node).is_some();
        let (shaped, hit) = if editable {
            text.shape_cached_editable(&content)
        } else {
            text.shape_cached(&content)
        };
        if hit {
            report.reused += 1;
        } else {
            report.shaped += 1;
        }
        if let Some(placeholder) = tree
            .editable(node)
            .and_then(|editable| editable.placeholder_content())
        {
            let (placeholder_shaped, _hit) = text.shape_cached_editable(&placeholder);
            if tree.editable_mut(node).is_some_and(|editable| {
                editable.ensure_placeholder(placeholder, placeholder_shaped)
            }) {
                tree.mark_text_layout_dirty(node);
            }
        }
        let key = if editable {
            crate::text::cache::ShapeKey::parley_only(&content, text.font_stack_version())
        } else {
            crate::text::cache::ShapeKey::new(&content, text.font_stack_version())
        };
        if let Some(slot) = tree.text_slot(node)
            && slot.key() == key
            && slot.dpi_epoch() == dpi_epoch
            && Rc::ptr_eq(slot.shaped(), &shaped)
        {
            report.skipped += 1;
            continue;
        }

        let measured = if editable {
            shaped.measure_editable(known, available)
        } else {
            shaped.measure(known, available)
        };
        let previous = tree.text_measured_size(node).unwrap_or(LogicalSize::ZERO);
        let fixed_layout = tree
            .get(node)
            .is_ok_and(|target| target.layout_override().is_some());
        let size_changed = !fixed_layout
            && (previous.width.value().to_bits() != measured.width.value().to_bits()
                || previous.height.value().to_bits() != measured.height.value().to_bits());
        tree.set_text_shaped(
            node,
            Rc::clone(&shaped),
            key,
            dpi_epoch,
            measured,
            size_changed,
            editable,
        );
    }
    let fast = text.take_fast_path_stats();
    report.fast_path_shaped = fast.hits;
    report.fast_path_queries = fast.queries;
    report.fast_path_ineligible = fast.ineligible();
    report.dirty_spans = pending_dirty.rects().len();
    Some(Ok(report))
}

pub(crate) fn layout_text(
    app: &mut App,
    window: WindowId,
) -> Option<Result<TextLayoutReport, EngineError>> {
    let App {
        text,
        raster,
        atlas,
        glyph_atlas,
        windows,
        pending_dirty,
        text_layout_scratch,
        ..
    } = app;
    let window_id = window;
    let scale = windows.get(&window_id)?.scale();
    let mut report = TextLayoutReport::default();
    let mut any_eviction = false;
    let mut scratch = Vec::new();
    glyph_atlas.begin_pass();
    {
        let window = windows.get_mut(&window_id)?;
        window
            .tree_mut()
            .take_text_layout_dirty(text_layout_scratch);
    }
    if text_layout_scratch.is_empty() {
        let _ = glyph_atlas.note_soft_fail(0);
        return Some(Ok(report));
    }

    let mut round = 0usize;
    loop {
        report.rasterized = 0;
        report.tiles_reused = 0;
        report.atlas_rebuilds = 0;
        report.atlas_evictions = 0;
        report.skipped_glyphs = 0;
        let generation = atlas.generation();
        let mut laid = 0usize;
        let mut reused = 0usize;
        let _previous_evictions = glyph_atlas.take_evicted();
        let window = windows.get_mut(&window_id)?;
        let tree = window.tree_mut();
        let mut index = 0usize;
        while index < text_layout_scratch.len() {
            let node = text_layout_scratch[index];
            index += 1;
            let Some(shaped) = tree.text_slot(node).map(|slot| Rc::clone(slot.shaped())) else {
                continue;
            };
            let width = tree
                .text_layout_width(node)
                .unwrap_or_else(|| shaped.max_content_size().width);
            let current = tree
                .text_slot(node)
                .map(|slot| (slot.laid_width_bits(), slot.laid_generation()));
            if current == Some((width.value().to_bits(), generation))
                && tree.text_line(node).is_some()
            {
                reused += 1;
            } else {
                let Some(content) = tree.text_content(node) else {
                    continue;
                };
                let color = content.style.color;
                let alignment = content.style.alignment;
                let ellipsis = text.ellipsis_for(&content.style);
                let mut ctx = crate::text::shape::RasterCtx {
                    raster,
                    glyph_atlas,
                    atlas,
                    generation,
                    scale,
                };
                let (line, mut diagnostics, stats) = match shaped.rasterize(
                    width,
                    alignment,
                    color,
                    &mut ctx,
                    ellipsis.as_deref(),
                ) {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                };
                report.rasterized += stats.rasters;
                report.tiles_reused += stats.hits;
                report.atlas_rebuilds += stats.rebuilds;
                report.atlas_evictions += stats.evictions;
                report.skipped_glyphs += stats.skipped;
                pending_dirty.merge(&mut atlas.take_dirty_rects());
                let soft_fail = stats.skipped > 0;
                if soft_fail {
                    diagnostics.push(TextDiagnostic::AtlasFull {
                        skipped: stats.skipped,
                    });
                    tree.mark_text_layout_dirty(node);
                }
                text.push_diagnostics(diagnostics);
                let laid_generation = if soft_fail {
                    PENDING_ATLAS_RETRY_GENERATION
                } else {
                    generation
                };
                tree.set_text_line(node, Rc::new(line), width, laid_generation);
                laid += 1;
            }
            let placeholder = tree.editable(node).and_then(|editable| {
                let (content, shaped) = editable.placeholder_layout()?;
                let placeholder_width = if width > LogicalPx::ZERO {
                    width
                } else {
                    shaped.max_content_size().width
                };
                (!editable.placeholder_line_fresh(generation, placeholder_width)).then_some((
                    content,
                    shaped,
                    placeholder_width,
                ))
            });
            if let Some((content, shaped, placeholder_width)) = placeholder {
                let ellipsis = text.ellipsis_for(&content.style);
                let mut ctx = crate::text::shape::RasterCtx {
                    raster,
                    glyph_atlas,
                    atlas,
                    generation,
                    scale,
                };
                let (line, diagnostics, stats) = match shaped.rasterize(
                    placeholder_width,
                    content.style.alignment,
                    content.style.color,
                    &mut ctx,
                    ellipsis.as_deref(),
                ) {
                    Ok(value) => value,
                    Err(error) => return Some(Err(error)),
                };
                report.rasterized += stats.rasters;
                report.tiles_reused += stats.hits;
                report.atlas_rebuilds += stats.rebuilds;
                report.atlas_evictions += stats.evictions;
                report.skipped_glyphs += stats.skipped;
                pending_dirty.merge(&mut atlas.take_dirty_rects());
                text.push_diagnostics(diagnostics);
                if stats.skipped > 0 {
                    tree.mark_text_layout_dirty(node);
                } else if let Some(editable) = tree.editable_mut(node) {
                    editable.set_placeholder_line(Rc::new(line), placeholder_width);
                }
            }
        }
        let evicted = glyph_atlas.take_evicted();
        let generation_changed = atlas.generation() != generation;
        report.laid = laid;
        report.reused = reused;
        report.reruns = round;
        if evicted == 0 && !generation_changed {
            any_eviction |= report.atlas_evictions > 0;
            break;
        }
        any_eviction = true;
        tree.invalidate_text_lines();
        tree.mark_canvas_nodes_paint();
        round += 1;
        if round >= MAX_LAYOUT_TEXT_PASSES {
            break;
        }
        scratch.clear();
        tree.take_text_layout_dirty(&mut scratch);
        std::mem::swap(&mut scratch, text_layout_scratch);
    }

    if any_eviction {
        for (id, other) in windows.iter_mut() {
            if *id == window_id {
                continue;
            }
            other.tree_mut().invalidate_text_lines();
            other.tree_mut().mark_canvas_nodes_paint();
        }
    }
    if let Err(error) = glyph_atlas.note_soft_fail(report.skipped_glyphs) {
        return Some(Err(error));
    }
    Some(Ok(report))
}

pub(crate) const MAX_LAYOUT_TEXT_PASSES: usize = 4;

const PENDING_ATLAS_RETRY_GENERATION: u32 = u32::MAX;

#[must_use]
pub(crate) fn font_stack(app: &App) -> &FontStack {
    app.text.font_stack()
}

#[must_use]
pub(crate) fn font_stack_version(app: &App) -> u32 {
    app.text.font_stack_version()
}

pub(crate) fn set_digit_fast_path(app: &mut App, enabled: bool) {
    app.text.set_digit_fast_path(enabled);
}

#[must_use]
pub(crate) fn digit_fast_path_enabled(app: &App) -> bool {
    app.text.digit_fast_path_enabled()
}

#[must_use]
pub(crate) fn text_diagnostics(app: &App) -> &[TextDiagnostic] {
    app.text.diagnostics()
}

pub(crate) fn font_stack_status(app: &mut App) -> FontStackStatus {
    app.text.font_stack_status()
}

pub(crate) fn set_font_stack(app: &mut App, stack: FontStack) -> u32 {
    let version = app.text.set_font_stack(stack);
    app.glyph_atlas.clear();
    app.atlas.clear();
    app.raster.set_font_epoch(app.text.font_epoch());
    app.invalidate_all_text();
    version
}

pub(crate) fn reload_system_fonts(app: &mut App) -> u32 {
    let version = app.text.reload_system_fonts();
    app.glyph_atlas.clear();
    app.atlas.clear();
    app.raster.set_font_epoch(app.text.font_epoch());
    app.invalidate_all_text();
    version
}

pub(crate) fn repaint_text_if_atlas_changed(app: &mut App) {
    let generation = app.atlas.generation();
    if generation == app.atlas_generation_seen {
        return;
    }
    app.atlas_generation_seen = generation;
    for window in app.windows.values_mut() {
        let tree = window.tree_mut();
        tree.invalidate_text_lines();
        tree.mark_canvas_nodes_paint();
    }
}

pub(crate) fn invalidate_all_text(app: &mut App) {
    let ids: SmallVec<[WindowId; 4]> = app.windows.keys().copied().collect();
    for id in ids {
        let Some(window) = app.windows.get_mut(&id) else {
            continue;
        };
        let tree = window.tree_mut();
        let mut nodes: Vec<crate::node::id::NodeId> = Vec::new();
        tree.for_each_dfs(|node| {
            if tree.text_content(node).is_some() {
                nodes.push(node);
            }
        });
        for node in nodes {
            tree.invalidate_text_layout(node);
        }
        tree.mark_all_text_dirty();
    }
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use super::MAX_LAYOUT_TEXT_PASSES;
    use crate::{
        app::App,
        error::EngineError,
        math::px,
        node::{builder::NodeBuilder, id::NodeId},
        paint::{atlas::AtlasKind, primitive::Primitive},
        render::AtlasAllocator,
        text::{
            InlineAlign, InlineBoxSpec, TextContent, TextDiagnostic, TextOverflow, TextRun,
            TextStyle, atlas::ATLAS_FULL_ESCALATION_FRAMES,
        },
        window::{WindowId, WindowOptions},
    };

    fn new_app() -> (tokio::runtime::Runtime, App) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let app = App::new(rt.handle().clone());
        (rt, app)
    }

    fn text_style(width: Option<f32>) -> crate::paint::style::Style {
        let mut style = crate::paint::style::Style::default();
        style.align_self(taffy::style::AlignSelf::START);
        if let Some(width) = width {
            style.width(taffy::style::Dimension::length(width));
        }
        style
    }

    fn text_app(
        text: &str,
        width: Option<f32>,
    ) -> (tokio::runtime::Runtime, App, WindowId, NodeId) {
        let (rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let w = app.window_mut(window).expect("窗口存在");
        let root = w.tree().root();
        let node = NodeBuilder::text(text, TextStyle::new(px(16.0)))
            .style(text_style(width))
            .build(w.tree_mut(), root)
            .expect("挂载成功");
        (rt, app, window, node)
    }

    fn frame_text(app: &mut App, window: WindowId) -> crate::text::TextLayoutReport {
        app.refresh_text(window)
            .expect("窗口存在")
            .expect("shape 不报错");
        app.window_mut(window)
            .expect("窗口存在")
            .compute_layout()
            .expect("布局不报错");
        app.layout_text(window)
            .expect("窗口存在")
            .expect("layout_text 不报错")
    }

    fn constrain_glyph_atlas(app: &mut App) {
        let mut atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (32, 32));
        atlas.cap_page_for_test(32);
        app.atlas = atlas;
        app.glyph_atlas.clear();
    }

    #[test]
    fn a_constrained_atlas_reports_skips_and_retries_next_frame() {
        let (_rt, mut app, window, node) =
            text_app("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789", Some(400.0));
        constrain_glyph_atlas(&mut app);

        let first = frame_text(&mut app, window);
        assert!(
            first.rasterized > 0,
            "本帧必须真的光栅（report: {first:?}）"
        );
        assert!(
            first.skipped_glyphs > 0,
            "32² 的页放不下 36 个不同字形（report: {first:?}）"
        );
        assert!(
            first.skipped_glyphs <= first.rasterized,
            "skipped 恒 <= rasterized（report: {first:?}）"
        );
        assert!(
            app.text_diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                TextDiagnostic::AtlasFull { skipped } if *skipped > 0
            )),
            "软失败必须可观测（diagnostics: {:?}）",
            app.text_diagnostics()
        );
        {
            let tree = app.window(window).expect("窗口存在").tree();
            assert_eq!(
                tree.text_layout_dirty_len(),
                1,
                "软失败的节点必须重新入下一帧的重排队列"
            );
            assert_ne!(
                tree.text_slot(node).expect("文本槽").laid_generation(),
                app.atlas.generation(),
                "软失败的行必须带「待重排」标记（否则下一帧会直接复用、永不补字形）"
            );
        }

        let retry = app
            .layout_text(window)
            .expect("窗口存在")
            .expect("第二次 pass 还没到升级阈值");
        assert_eq!(
            (retry.laid, retry.reused),
            (1, 0),
            "软失败的行下一帧必须真正重光栅重试（report: {retry:?}）"
        );
        assert!(
            retry.skipped_glyphs > 0,
            "容量没变，重试仍放不下（report: {retry:?}）"
        );

        assert_eq!(ATLAS_FULL_ESCALATION_FRAMES, 3, "阈值是「连续 3 个 pass」");
        let third = app.layout_text(window).expect("窗口存在");
        let error = third.expect_err("连续软失败达到阈值必须升级为错误");
        assert!(
            matches!(
                error,
                EngineError::AtlasExhausted {
                    kind: AtlasKind::Glyph
                }
            ),
            "必须报 AtlasExhausted/Glyph，实际 {error:?}"
        );
        let fourth = app
            .layout_text(window)
            .expect("窗口存在")
            .expect("升级后计数清零，下一个 pass 不得再报错");
        assert!(
            fourth.skipped_glyphs > 0,
            "容量仍未恢复（report: {fourth:?}）"
        );
    }

    #[test]
    fn an_ample_atlas_reports_zero_skipped_glyphs() {
        let (_rt, mut app, window, _node) = text_app("hello", Some(200.0));

        let report = frame_text(&mut app, window);
        assert!(
            report.rasterized > 0,
            "本帧必须真的光栅（report: {report:?}）"
        );
        assert_eq!(
            report.skipped_glyphs, 0,
            "512² 的页放得下 5 个字形（report: {report:?}）"
        );
        assert!(
            !app.text_diagnostics()
                .iter()
                .any(|diagnostic| matches!(diagnostic, TextDiagnostic::AtlasFull { .. })),
            "没有软失败就不该有 AtlasFull 诊断（diagnostics: {:?}）",
            app.text_diagnostics()
        );
        let tree = app.window(window).expect("窗口存在").tree();
        assert_eq!(tree.text_layout_dirty_len(), 0, "没有软失败就不该重新入队");
    }

    #[test]
    fn normalized_runs_emit_one_invalid_text_run_diagnostic() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let content = TextContent::new("a界b", TextStyle::new(px(16.0))).with_runs([
            TextRun::new(0..1, TextStyle::new(px(16.0))),
            TextRun::new(1..2, TextStyle::new(px(16.0))),
            TextRun::new(4..6, TextStyle::new(px(16.0))),
        ]);
        assert_eq!(content.normalized_runs(), 2, "构造期两个 run 被改动");
        let node = {
            let w = app.window_mut(window).expect("窗口存在");
            let root = w.tree().root();
            let node = NodeBuilder::text("", TextStyle::new(px(16.0)))
                .style(text_style(Some(200.0)))
                .build(w.tree_mut(), root)
                .expect("挂载成功");
            assert!(w.tree_mut().set_text_content(node, content), "文本载荷");
            node
        };
        let _ = frame_text(&mut app, window);
        let invalid: Vec<_> = app
            .text_diagnostics()
            .iter()
            .filter(|diagnostic| matches!(diagnostic, TextDiagnostic::InvalidTextRun { .. }))
            .cloned()
            .collect();
        assert_eq!(
            invalid,
            vec![TextDiagnostic::InvalidTextRun { runs: 2 }],
            "归一化计数必须成为可观测诊断（diagnostics: {:?}）",
            app.text_diagnostics()
        );
        assert!(
            app.window(window)
                .expect("窗口存在")
                .tree()
                .text_line(node)
                .is_some(),
            "归一化后的 runs 仍必须正常成形（不失败）"
        );

        assert!(app.atlas.grow(), "512² → 1024² 必须成功");
        app.repaint_text_if_atlas_changed();
        let _ = app
            .layout_text(window)
            .expect("窗口存在")
            .expect("重排不报错");
        let count = app
            .text_diagnostics()
            .iter()
            .filter(|diagnostic| matches!(diagnostic, TextDiagnostic::InvalidTextRun { runs: 2 }))
            .count();
        assert_eq!(
            count,
            1,
            "同一诊断去重，不随 pass 刷屏: {:?}",
            app.text_diagnostics()
        );
    }

    #[test]
    fn a_retried_line_fills_the_missing_glyphs_once_capacity_returns() {
        let text = "ABCDEFGHIJ0123456789";
        let (_rt, mut app, window, node) = text_app(text, Some(400.0));
        constrain_glyph_atlas(&mut app);
        let first = frame_text(&mut app, window);
        assert!(
            first.skipped_glyphs > 0,
            "受限图集必须触发软失败（report: {first:?}）"
        );

        app.atlas = AtlasAllocator::with_page(AtlasKind::Glyph, (512, 512));
        app.glyph_atlas.clear();
        let retried = app
            .layout_text(window)
            .expect("窗口存在")
            .expect("容量恢复后不得报错");
        assert_eq!(
            retried.skipped_glyphs, 0,
            "容量恢复后不得再软失败（report: {retried:?}）"
        );
        assert!(
            retried.laid >= 1,
            "软失败的行必须被重排（report: {retried:?}）"
        );
        let line = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .expect("行快照")
            .clone();
        let drawn: usize = line.glyph_runs().map(|run| run.glyphs.len()).sum();
        assert_eq!(
            drawn,
            text.chars().count(),
            "重试后必须把少画的字形全部补上（重排轮数上限 {MAX_LAYOUT_TEXT_PASSES}）"
        );
        assert_eq!(line.atlas_generation, app.atlas.generation());
    }

    #[test]
    fn atlas_generation_change_invalidates_lines_through_the_unique_entry() {
        let (_rt, mut app, window, node) = text_app("1,234.56", Some(200.0));
        let _ = frame_text(&mut app, window);
        let old_generation = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .expect("行快照")
            .atlas_generation;
        assert_eq!(old_generation, app.atlas.generation());

        assert!(app.atlas.grow(), "512² → 1024² 必须成功");
        app.repaint_text_if_atlas_changed();
        {
            let tree = app.window(window).expect("窗口存在").tree();
            assert!(
                tree.text_line(node).is_none(),
                "换代必须清掉旧 ShapedLine（否则 paint 会重编旧 UV）"
            );
            assert_eq!(tree.text_layout_dirty_len(), 1, "节点必须进入重排队列");
        }

        let report = app
            .layout_text(window)
            .expect("窗口存在")
            .expect("重排不报错");
        assert!(report.laid >= 1, "必须真正重光栅（report: {report:?}）");
        let line = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .expect("行快照")
            .clone();
        assert_eq!(
            line.atlas_generation,
            app.atlas.generation(),
            "重光栅必须使用当前图集代际"
        );
        assert_ne!(line.atlas_generation, old_generation, "换代后必须换新代际");
    }
    #[test]
    fn stale_text_line_is_skipped_by_paint_and_healed_next_frame() {
        let (_rt, mut app, window, node) = text_app("1,234.56", Some(200.0));

        app.repaint_text_if_atlas_changed();
        let _ = frame_text(&mut app, window);
        let width = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_content_width(node);
        let current = app.atlas.generation();
        let stale = {
            let existing = app
                .window(window)
                .expect("窗口存在")
                .tree()
                .text_line(node)
                .expect("行快照")
                .clone();
            let mut line = (*existing).clone();
            line.atlas_generation = current.wrapping_add(1);
            Rc::new(line)
        };
        assert!(
            app.window_mut(window)
                .expect("窗口存在")
                .tree_mut()
                .set_text_line(node, stale, width, current)
        );

        let requests = app.frame_requests();
        let _ = app.frame(window).expect("帧").expect("首帧不报错");
        {
            let target = app.window(window).expect("窗口存在");
            let encoded = target
                .tree()
                .lane_of(node)
                .and_then(|lane| target.scene().lane_primitives(lane))
                .is_some_and(|primitives| {
                    primitives
                        .iter()
                        .any(|primitive| matches!(primitive, Primitive::GlyphRun(_)))
                });
            assert!(!encoded, "旧代际行本帧不得编码 GlyphRun（旧 UV）");
        }
        assert!(
            app.frame_requests() > requests,
            "硬校验必须请求下一帧补齐（stale_text 处理）"
        );

        let _ = app.frame(window).expect("帧").expect("补齐帧不报错");
        let line = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .expect("行快照")
            .clone();
        assert_eq!(
            line.atlas_generation,
            app.atlas.generation(),
            "下一帧必须用当前图集代际重光栅"
        );
        assert!(
            line.glyph_runs().any(|run| !run.glyphs.is_empty()),
            "重光栅后必须真的有字形（不是永久跳过）"
        );
    }

    #[test]
    fn a_node_text_inline_box_lands_in_the_shaped_line() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let spec = InlineBoxSpec {
            id: 7,
            index: 1,
            size: crate::math::LogicalSize::new(px(12.0), px(12.0)),
            baseline: None,
            align: InlineAlign::Baseline,
            paint: None,
        };
        let node = {
            let w = app.window_mut(window).expect("窗口存在");
            let root = w.tree().root();
            NodeBuilder::text("a b", TextStyle::new(px(16.0)))
                .inline_boxes([spec])
                .style(text_style(Some(200.0)))
                .build(w.tree_mut(), root)
                .expect("挂载成功")
        };
        let report = frame_text(&mut app, window);
        assert!(report.laid > 0, "the text node must be laid out");
        let line = app
            .window(window)
            .expect("窗口存在")
            .tree()
            .text_line(node)
            .expect("laid line")
            .clone();
        let inline = line
            .lines
            .iter()
            .flat_map(|shaped| shaped.items.iter())
            .find_map(|item| match item {
                crate::text::ShapedTextItem::InlineBox(inline) => Some(inline),
                crate::text::ShapedTextItem::GlyphRun(_) => None,
            })
            .expect("the inline box lands in the shaped line");
        assert_eq!(inline.id, 7);
        assert_eq!(
            inline.rect.size,
            crate::math::LogicalSize::new(px(12.0), px(12.0)),
            "the inline box keeps its requested size"
        );
    }

    #[test]
    fn a_node_text_ellipsis_truncates_to_the_max_lines() {
        let (_rt, mut app) = new_app();
        let window = app.create_window(WindowOptions::default());
        let mut ellipsis_style = TextStyle::new(px(16.0));
        ellipsis_style.overflow = TextOverflow::Ellipsis;
        ellipsis_style.max_lines = 1;
        let mut clip_style = TextStyle::new(px(16.0));
        clip_style.overflow = TextOverflow::Clip;
        let long = "the quick brown fox jumps over the lazy dog";
        let node = {
            let w = app.window_mut(window).expect("窗口存在");
            let root = w.tree().root();
            NodeBuilder::text(long, ellipsis_style)
                .style(text_style(Some(120.0)))
                .build(w.tree_mut(), root)
                .expect("挂载成功")
        };
        let control = {
            let w = app.window_mut(window).expect("窗口存在");
            let root = w.tree().root();
            NodeBuilder::text(long, clip_style)
                .style(text_style(Some(120.0)))
                .build(w.tree_mut(), root)
                .expect("挂载成功")
        };
        frame_text(&mut app, window);
        let ellipsis = {
            let tree = app.window(window).expect("窗口存在").tree();
            (
                tree.text_line(node).expect("laid line").clone(),
                tree.text_line(control).expect("control line").clone(),
            )
        };
        assert!(
            ellipsis.1.lines.len() > 1,
            "the control text wraps past the max visible lines"
        );
        assert_eq!(
            ellipsis.0.lines.len(),
            1,
            "ellipsis truncates the visible line count"
        );
        assert!(
            ellipsis.0.width <= px(121.0),
            "the ellipsized line stays inside the measure width"
        );
        assert!(
            ellipsis.0.glyph_runs().any(|run| !run.glyphs.is_empty()),
            "the truncated line still carries glyphs"
        );
        assert_eq!(
            ellipsis.0.text.as_str(),
            long,
            "the shaped line keeps the logical text"
        );
    }
}
