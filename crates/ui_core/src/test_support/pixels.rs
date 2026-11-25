use crate::{
    app::App,
    error::EngineError,
    math::{DevicePixelRatio, DeviceRect, LogicalPoint},
    paint::atlas::AtlasKind,
    render::{
        DirtyRects,
        backend::compositor::GpuRenderer,
        compositor::{FramePlan, Plan},
        delta::{ScrollDelta, ScrollTable},
        frame::FramePayload,
        timer::FrameTimings,
    },
    test_support::snapshot::{SnapshotError, SnapshotImage},
    window::{FrameIndex, Window},
};

pub type ScrollSlot = (LogicalPoint, DeviceRect);

pub fn render_offscreen(
    app: &App,
    window: &Window,
    size: (u32, u32),
    scale: DevicePixelRatio,
    scroll: &[ScrollSlot],
) -> Result<SnapshotImage, EngineError> {
    let (offsets, clip) = scroll_payload(scroll, scale);
    let mut renderer = offscreen_gpu(size)?;
    let (glyph_page, glyph_pixels) = app.glyph_page_pixels();
    renderer.queue_atlas_upload(
        AtlasKind::Glyph,
        0,
        crate::render::AtlasArraySpec {
            page: glyph_page.0,
            layers: 1,
        },
        glyph_pixels,
        DirtyRects::full(),
    );
    if let Some((color_page, color_pixels)) = app.color_glyph_page_pixels() {
        renderer.queue_atlas_upload(
            AtlasKind::ColorGlyph,
            0,
            crate::render::AtlasArraySpec {
                page: color_page.0,
                layers: 1,
            },
            color_pixels,
            DirtyRects::full(),
        );
    }
    let image_pages = app.resource_page_pixels_all();
    let image_layers = u32::try_from(image_pages.len()).expect("image page count fits u32");
    for (layer, (image_page, image_pixels)) in image_pages.into_iter().enumerate() {
        renderer.queue_atlas_upload(
            AtlasKind::Image,
            layer as u16,
            crate::render::AtlasArraySpec {
                page: image_page.0,
                layers: image_layers,
            },
            image_pixels.to_vec(),
            DirtyRects::full(),
        );
    }
    let buffer = window
        .instance_buffer()
        .ok_or_else(|| EngineError::AppInit {
            detail: smol_str::SmolStr::new_static(
                "pixel readback requires an inline (headless) window",
            ),
        })?;
    let payload = FramePayload::full(buffer, offsets, clip);
    let mut frame = FramePlan::new(Plan::Full, true);
    let mut timings = FrameTimings::new();
    renderer.submit_snapshot(FrameIndex::from_raw(1), &payload, &mut frame, &mut timings)?;
    let image = renderer.readback()?;
    Ok(SnapshotImage {
        width: image.width,
        height: image.height,
        rgba8: image.rgba8,
    })
}

fn offscreen_gpu(size: (u32, u32)) -> Result<GpuRenderer, EngineError> {
    #[cfg(windows)]
    {
        GpuRenderer::new_warp_offscreen(size)
    }
    #[cfg(not(windows))]
    {
        GpuRenderer::new_offscreen(size)
    }
}

fn scroll_payload(
    scroll: &[ScrollSlot],
    scale: DevicePixelRatio,
) -> (Vec<[f32; 2]>, Vec<[i32; 4]>) {
    let mut table = ScrollTable::new();
    let deltas: Vec<ScrollDelta> = scroll
        .iter()
        .enumerate()
        .map(|(slot, (offset, viewport))| ScrollDelta {
            slot: slot as u32,
            offset: *offset,
            viewport: *viewport,
        })
        .collect();
    table.apply(&deltas, scale);
    let mut offsets = Vec::with_capacity(scroll.len());
    let mut clip = Vec::with_capacity(scroll.len());
    table.to_payload(scale, &mut offsets);
    table.viewports_to_payload(&mut clip);
    (offsets, clip)
}

#[must_use]
pub fn pixel(image: &SnapshotImage, x: u32, y: u32) -> [u8; 4] {
    assert!(
        x < image.width && y < image.height,
        "pixel ({x},{y}) out of bounds ({}x{})",
        image.width,
        image.height
    );
    let offset = ((y as usize) * (image.width as usize) + (x as usize)) * 4;
    image.rgba8[offset..offset + 4]
        .try_into()
        .expect("4 bytes per pixel")
}

#[must_use]
pub fn channel_span(a: [u8; 4], b: [u8; 4]) -> u8 {
    a.iter()
        .zip(b.iter())
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0)
}

#[must_use]
pub fn region_max_span(
    image: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    expected: [u8; 4],
) -> u8 {
    let mut span = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            span = span.max(channel_span(pixel(image, x, y), expected));
        }
    }
    span
}

#[must_use]
pub fn region_diff(
    before: &SnapshotImage,
    after: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
) -> u8 {
    assert_eq!(
        (before.width, before.height),
        (after.width, after.height),
        "readback sizes must not drift between frames"
    );
    let mut span = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            span = span.max(channel_span(pixel(before, x, y), pixel(after, x, y)));
        }
    }
    span
}

#[must_use]
pub fn count_pixels(
    image: &SnapshotImage,
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    predicate: impl Fn([u8; 4]) -> bool,
) -> u32 {
    let mut count = 0;
    for y in y0..=y1 {
        for x in x0..=x1 {
            if predicate(pixel(image, x, y)) {
                count += 1;
            }
        }
    }
    count
}

#[must_use]
pub fn describe_error(error: &SnapshotError) -> String {
    match error {
        SnapshotError::NoRenderer => String::from("no renderer in this build"),
        SnapshotError::Gpu { detail } => format!("gpu: {detail}"),
    }
}
