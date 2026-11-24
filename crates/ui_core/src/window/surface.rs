use super::{FramePresentation, Window, WindowOptions};
use crate::{
    error::EngineError,
    math::{DevicePixelRatio, DeviceRect, LogicalRect, LogicalSize, dp, px},
    paint::{atlas::AtlasKind, color::Color},
    render::backend::Renderer,
};

const MAX_ATLAS_FOLLOWUPS: u8 = 3;

pub(crate) fn options(window: &Window) -> &WindowOptions {
    &window.options
}

pub(crate) fn set_background_color(window: &mut Window, color: Color) {
    if window.options.background == color {
        return;
    }
    window.options.background = color;
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    {
        window.full_damage_pending = true;
        if let Some(renderer) = window.renderer.as_mut() {
            renderer.set_background_color(color);
        }
    }
}

pub(crate) fn size(window: &Window) -> LogicalSize {
    window.options.size
}

pub(crate) fn set_size(window: &mut Window, size: LogicalSize) {
    if window.size() == size {
        return;
    }
    window.options.size = size;
    window.full_damage_pending = true;
    let root = window.tree.root();
    window.tree.invalidate_layout(root);
    if !window.surface_from_platform {
        window.surface = covering_surface(window.size(), window.scale);
    }
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
pub(crate) fn set_surface(window: &mut Window, surface: (u32, u32)) {
    let surface = (surface.0.max(1), surface.1.max(1));
    window.surface_from_platform = true;
    #[cfg(target_os = "macos")]
    if let Some(renderer) = window.renderer.as_mut() {
        renderer.set_layer_scale(surface, window.scale.factor());
    }
    if window.surface == surface {
        return;
    }
    window.surface = surface;
    window.full_damage_pending = true;
}

#[cfg(target_os = "macos")]
pub(crate) fn set_max_frame_rate(window: &mut Window, max_frame_rate: f32) {
    if let Some(renderer) = window.renderer.as_mut() {
        renderer.set_frame_rate(max_frame_rate);
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn set_buffer_scale(window: &mut Window, scale: i32) {
    window.buffer_scale = scale.max(1);
}

pub(crate) fn has_renderer(window: &Window) -> bool {
    window.renderer.is_some()
}

#[cfg(test)]
pub(crate) fn glyph_page_received(window: &Window) -> bool {
    window.glyph_page_received.is_some()
}

#[cfg(test)]
pub(crate) fn image_page_received(window: &Window) -> bool {
    window.image_page_received.is_some()
}

pub(crate) fn image_layer_received(window: &Window, layer: u16) -> bool {
    window
        .image_layers_received
        .get(usize::from(layer))
        .copied()
        .unwrap_or(false)
}

pub(crate) fn atlas_page_matches(
    window: &Window,
    kind: AtlasKind,
    spec: crate::render::AtlasArraySpec,
) -> bool {
    match kind {
        AtlasKind::Glyph => window.glyph_page_received == Some(spec.page),
        AtlasKind::Image => window.image_page_received == Some(spec),
    }
}

pub(crate) fn mark_atlas_page_received(
    window: &mut Window,
    kind: AtlasKind,
    spec: crate::render::AtlasArraySpec,
) {
    match kind {
        AtlasKind::Glyph => window.glyph_page_received = Some(spec.page),
        AtlasKind::Image => window.image_page_received = Some(spec),
    }
}

pub(crate) fn mark_image_layer_received(window: &mut Window, layer: u16) {
    if let Some(slot) = window.image_layers_received.get_mut(usize::from(layer)) {
        *slot = true;
    }
}

pub(crate) fn mark_atlas_page_stale(window: &mut Window, kind: AtlasKind) {
    match kind {
        AtlasKind::Glyph => {
            window.glyph_page_received = None;
            window.glyph_atlas_rects.clear();
        }
        AtlasKind::Image => {
            window.image_page_received = None;
            window.image_layers_received = [false; crate::paint::atlas::MAX_IMAGE_PAGES as usize];
            window.image_drawable_layers = 0;
            for rects in &mut window.image_atlas_rects {
                rects.clear();
            }
        }
    }
}

pub(crate) fn atlas_rects(
    window: &Window,
    kind: AtlasKind,
    layer: u16,
) -> &crate::render::DirtyRects {
    debug_assert!(
        u32::from(layer) < per_window_layer_slots(kind),
        "atlas layer {layer} out of range for {kind:?}"
    );
    match kind {
        AtlasKind::Glyph => &window.glyph_atlas_rects,
        AtlasKind::Image => &window.image_atlas_rects[usize::from(layer)],
    }
}

fn per_window_layer_slots(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Glyph => 1,
        AtlasKind::Image => u32::from(crate::paint::atlas::MAX_IMAGE_PAGES),
    }
}

pub(crate) fn atlas_rects_mut(
    window: &mut Window,
    kind: AtlasKind,
    layer: u16,
) -> &mut crate::render::DirtyRects {
    debug_assert!(
        u32::from(layer) < per_window_layer_slots(kind),
        "atlas layer {layer} out of range for {kind:?}"
    );
    match kind {
        AtlasKind::Glyph => &mut window.glyph_atlas_rects,
        AtlasKind::Image => &mut window.image_atlas_rects[usize::from(layer)],
    }
}

pub(crate) fn clear_atlas_rects(window: &mut Window, kind: AtlasKind, layer: u16) {
    atlas_rects_mut(window, kind, layer).clear();
}

pub(crate) fn clear_all_atlas_rects(window: &mut Window, kind: AtlasKind) {
    match kind {
        AtlasKind::Glyph => window.glyph_atlas_rects.clear(),
        AtlasKind::Image => {
            for rects in &mut window.image_atlas_rects {
                rects.clear();
            }
        }
    }
}

pub(crate) fn atlas_rects_pending(window: &Window) -> bool {
    !window.glyph_atlas_rects.is_empty()
        || window
            .image_atlas_rects
            .iter()
            .any(|rects| !rects.is_empty())
}

pub(crate) fn note_atlas_deferral(window: &mut Window) -> bool {
    if window.atlas_deferrals >= MAX_ATLAS_FOLLOWUPS {
        return false;
    }
    window.atlas_deferrals += 1;
    true
}

pub(crate) fn reset_atlas_deferrals(window: &mut Window) {
    window.atlas_deferrals = 0;
}

pub(crate) fn queue_atlas_upload(
    window: &mut Window,
    kind: AtlasKind,
    layer: u16,
    spec: crate::render::AtlasArraySpec,
    pixels: &[u8],
    rects: crate::render::DirtyRects,
) -> bool {
    #[cfg(all(test, windows))]
    if window
        .atlas_upload_failure_from
        .is_some_and(|from| layer >= from)
    {
        return false;
    }
    window
        .renderer
        .as_mut()
        .is_some_and(|renderer| renderer.queue_atlas_upload(kind, layer, spec, pixels, rects))
}

pub(crate) fn atlas_idle(window: &Window, kind: AtlasKind) -> bool {
    window
        .renderer
        .as_ref()
        .is_none_or(|renderer| renderer.atlas_idle(kind))
}

#[cfg(all(test, windows))]
pub(crate) fn atlas_pending_layers(window: &Window, kind: AtlasKind) -> Option<Vec<u16>> {
    window
        .renderer
        .as_ref()
        .and_then(|renderer| renderer.atlas_pending_layers(kind))
}

#[cfg(target_os = "windows")]
pub(crate) fn request_composition_rebuild(window: &Window) {
    if let Some(renderer) = window.renderer.as_ref() {
        renderer.request_composition_rebuild();
    }
}

pub(crate) fn set_renderer(window: &mut Window, renderer: Renderer) {
    window.renderer = Some(renderer);
    reset_atlas_accounting(window);
}

#[cfg(target_os = "macos")]
pub(crate) fn take_renderer(window: &mut Window) -> Option<Renderer> {
    window.renderer.take()
}

#[cfg(target_os = "macos")]
pub(crate) fn has_compositor_ticket(window: &Window) -> bool {
    window.compositor_ticket.is_some()
}

pub(crate) fn poll_renderer(window: &mut Window) -> Result<(), EngineError> {
    let Some(renderer) = window.renderer.as_mut() else {
        return Ok(());
    };
    let feedback = renderer.poll()?;
    let Some(feedback) = feedback else {
        return Ok(());
    };
    let published_at = feedback
        .completed_at
        .checked_sub(feedback.latency)
        .unwrap_or(feedback.completed_at);
    let presentation = FramePresentation {
        index: feedback.index,
        published_at,
        completed_at: feedback.completed_at,
        presented_at: feedback.presented_at,
        latency: feedback.latency,
    };
    #[cfg(feature = "test-support")]
    if let Some(record) = window.record.as_mut()
        && record.frame_index == feedback.index
    {
        record.presented_latency = presentation.publish_to_presented();
    }
    window.note_frame_presentation(presentation);

    #[cfg(feature = "hud")]
    super::record::note_completed_frame(window, &feedback);
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn sync_pacing(window: &mut Window, continuous: bool) {
    if let Some(renderer) = window.renderer.as_mut() {
        renderer.sync_pacing(continuous);
    }
}

pub(crate) fn enable_offscreen_renderer(window: &mut Window) -> Result<(), EngineError> {
    if window.renderer.is_some() {
        return Ok(());
    }
    let surface = window.surface;
    window.renderer = Some(Renderer::new_offscreen(surface)?);
    reset_atlas_accounting(window);
    Ok(())
}

fn reset_atlas_accounting(window: &mut Window) {
    window.glyph_page_received = None;
    window.image_page_received = None;
    window.image_layers_received = [false; crate::paint::atlas::MAX_IMAGE_PAGES as usize];
    window.image_drawable_layers = 0;
    window.glyph_atlas_rects = crate::render::DirtyRects::default();
    window.image_atlas_rects = std::array::from_fn(|_| crate::render::DirtyRects::default());
}

#[cfg(windows)]
pub(crate) fn presented_surface(window: &Window) -> (u32, u32) {
    match window.renderer.as_ref() {
        Some(renderer) => renderer.presented_surface(),
        None => window.surface,
    }
}

#[cfg(windows)]
pub(crate) fn take_composition_pending(window: &Window) -> bool {
    window
        .renderer
        .as_ref()
        .is_some_and(Renderer::take_composition_pending)
}

pub(crate) fn frame_rect(window: &Window) -> DeviceRect {
    DeviceRect::new(
        dp(0),
        dp(0),
        dp(window.surface.0 as i32),
        dp(window.surface.1 as i32),
    )
}

pub(crate) fn covering_surface(size: LogicalSize, scale: DevicePixelRatio) -> (u32, u32) {
    let rect =
        scale.to_covering_device_rect(LogicalRect::new(px(0.0), px(0.0), size.width, size.height));
    (
        rect.width().value().max(1) as u32,
        rect.height().value().max(1) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{paint::atlas::MAX_IMAGE_PAGES, render::AtlasArraySpec, window::WindowId};

    fn window() -> Window {
        Window::new(WindowId::for_test(1), WindowOptions::default())
    }

    fn one_rect() -> crate::render::DirtyRects {
        let mut rects = crate::render::DirtyRects::default();
        rects.mark(DeviceRect::new(dp(1), dp(2), dp(3), dp(4)));
        rects
    }

    fn image_spec(page: u32, layers: u32) -> AtlasArraySpec {
        AtlasArraySpec { page, layers }
    }

    #[test]
    fn atlas_rects_are_tracked_per_texture_array_layer() {
        let mut window = window();
        window
            .atlas_rects_mut(AtlasKind::Image, 2)
            .merge_from(&one_rect());

        assert!(
            window.atlas_rects(AtlasKind::Image, 0).is_empty(),
            "layer 0 不得看到 layer 2 的矩形"
        );
        assert!(!window.atlas_rects(AtlasKind::Image, 2).is_empty());
        assert!(window.atlas_rects(AtlasKind::Glyph, 0).is_empty());
        assert!(window.atlas_rects_pending(), "任一层有矩形即待上传");

        window.clear_atlas_rects(AtlasKind::Image, 2);
        assert!(!window.atlas_rects_pending());
    }

    #[test]
    fn marking_the_image_atlas_stale_clears_every_layer() {
        let mut window = window();
        for layer in 0..MAX_IMAGE_PAGES {
            window
                .atlas_rects_mut(AtlasKind::Image, layer)
                .merge_from(&one_rect());
            window.mark_image_layer_received(layer);
        }
        window.mark_atlas_page_received(AtlasKind::Image, image_spec(64, 4));
        assert!(window.atlas_page_matches(AtlasKind::Image, image_spec(64, 4)));

        window.mark_atlas_page_stale(AtlasKind::Image);

        assert!(!window.atlas_page_matches(AtlasKind::Image, image_spec(64, 4)));
        assert!(!window.atlas_rects_pending(), "全部层的矩形必须清空");
        for layer in 0..MAX_IMAGE_PAGES {
            assert!(
                !window.image_layer_received(layer),
                "layer {layer} 的整层记账必须作废"
            );
            assert!(window.atlas_rects(AtlasKind::Image, layer).is_empty());
        }
    }

    #[test]
    fn a_grown_image_array_no_longer_matches_the_received_spec() {
        let mut window = window();
        window.mark_atlas_page_received(AtlasKind::Image, image_spec(2048, 1));
        assert!(window.atlas_page_matches(AtlasKind::Image, image_spec(2048, 1)));
        assert!(
            !window.atlas_page_matches(AtlasKind::Image, image_spec(2048, 2)),
            "层数 1 → 2 必须判定为未建立（要扩层 + 整组重传）"
        );
        window.mark_atlas_page_received(AtlasKind::Glyph, image_spec(512, 1));
        assert!(window.atlas_page_matches(AtlasKind::Glyph, image_spec(512, 1)));
        assert!(!window.atlas_page_matches(AtlasKind::Glyph, image_spec(1024, 1)));
    }
}
