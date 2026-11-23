use smol_str::SmolStr;

use crate::{
    math::PhysicalSize,
    resources::{
        DecodeCause,
        image::{DecodedImage, MAX_TILE_DIM, SourceColor, check_source_size},
    },
};

const SVG_FORMAT: &str = "svg";

pub(crate) fn probe_dimensions(bytes: &[u8]) -> Result<(u32, u32), DecodeCause> {
    let tree = parse(bytes)?;
    let size = tree.size();
    let width = pixel(size.width());
    let height = pixel(size.height());
    check_source_size(width, height)?;
    Ok((width, height))
}

pub(crate) fn resolve_target(target: PhysicalSize, intrinsic: (u32, u32)) -> (u32, u32) {
    let width = if target.width.is_finite() && target.width.value() > 0.0 {
        target.width.value()
    } else {
        intrinsic.0 as f32
    };
    let height = if target.height.is_finite() && target.height.value() > 0.0 {
        target.height.value()
    } else {
        intrinsic.1 as f32
    };
    let scale = (width / intrinsic.0 as f32).min(height / intrinsic.1 as f32);
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let width = (intrinsic.0 as f32 * scale).round();
    let height = (intrinsic.1 as f32 * scale).round();
    (
        (width.clamp(1.0, MAX_TILE_DIM as f32)) as u32,
        (height.clamp(1.0, MAX_TILE_DIM as f32)) as u32,
    )
}

pub(crate) fn decode(
    bytes: &[u8],
    target: Option<(u32, u32)>,
) -> Result<DecodedImage, DecodeCause> {
    let tree = parse(bytes)?;
    let size = tree.size();
    let (width, height) = match target {
        Some(target) => target,
        None => (
            pixel(size.width()).min(MAX_TILE_DIM),
            pixel(size.height()).min(MAX_TILE_DIM),
        ),
    };
    if width == 0 || height == 0 {
        return Err(DecodeCause::Corrupt {
            format: SmolStr::new_static(SVG_FORMAT),
            detail: SmolStr::new_static("svg has a zero target size"),
        });
    }
    let limit_bytes = u64::from(width) * u64::from(height) * 4;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or(DecodeCause::OutOfMemory { limit_bytes })?;
    let scale_x = width as f32 / size.width().max(f32::MIN_POSITIVE);
    let scale_y = height as f32 / size.height().max(f32::MIN_POSITIVE);
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

fn pixel(value: f32) -> u32 {
    if value.is_finite() && value > 0.0 {
        (value.ceil() as u32).max(1)
    } else {
        1
    }
}
