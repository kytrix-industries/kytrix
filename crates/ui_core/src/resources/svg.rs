use smol_str::SmolStr;

use crate::{
    math::DeviceSize,
    resources::{
        DecodeCause,
        image::{DecodedImage, MAX_TILE_DIM, SourceColor},
    },
};

const SVG_FORMAT: &str = "svg";

pub(crate) fn check_source(bytes: &[u8]) -> Result<(), DecodeCause> {
    let tree = parse(bytes)?;
    let size = tree.size();
    if !(size.width().is_finite()
        && size.height().is_finite()
        && size.width() > 0.0
        && size.height() > 0.0)
    {
        return Err(DecodeCause::Corrupt {
            format: SmolStr::new_static(SVG_FORMAT),
            detail: SmolStr::new_static("svg declares a zero or non-finite size"),
        });
    }
    Ok(())
}

pub(crate) fn decode(bytes: &[u8], target: DeviceSize) -> Result<DecodedImage, DecodeCause> {
    let tree = parse(bytes)?;
    let width = pixel_side(target.width.value()).min(MAX_TILE_DIM);
    let height = pixel_side(target.height.value()).min(MAX_TILE_DIM);
    let document = tree.size();
    let limit_bytes = u64::from(width) * u64::from(height) * 4;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or(DecodeCause::OutOfMemory { limit_bytes })?;
    let scale_x = width as f32 / document.width().max(f32::MIN_POSITIVE);
    let scale_y = height as f32 / document.height().max(f32::MIN_POSITIVE);
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale_x, scale_y),
        &mut pixmap.as_mut(),
    );
    let mut bgra = Vec::with_capacity(width as usize * height as usize * 4);
    for pixel in pixmap.pixels() {
        bgra.extend_from_slice(&[pixel.blue(), pixel.green(), pixel.red(), pixel.alpha()]);
    }
    Ok(DecodedImage {
        width,
        height,
        bgra,
        color: SourceColor::Srgb,
    })
}

fn parse(bytes: &[u8]) -> Result<usvg::Tree, DecodeCause> {
    usvg::Tree::from_data(bytes, &usvg::Options::default()).map_err(|error| DecodeCause::Corrupt {
        format: SmolStr::new_static(SVG_FORMAT),
        detail: SmolStr::new(error.to_string()),
    })
}

fn pixel_side(value: i32) -> u32 {
    if value <= 0 { 1 } else { (value as u32).max(1) }
}
