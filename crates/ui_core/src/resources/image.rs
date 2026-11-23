use std::{io::Cursor, sync::OnceLock};

use image::{
    DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, error::LimitErrorKind,
    imageops::FilterType,
};
use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions, Xyzd};
use smol_str::{SmolStr, format_smolstr};

use crate::{math::Size, resources::DecodeCause};

pub(crate) const MAX_TILE_DIM: u32 = 2046;

pub(crate) const MAX_SOURCE_DIM: u32 = 8192;

pub(crate) const MAX_SOURCE_PIXELS: u64 = 4096 * 4096;

pub(crate) const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;

pub(crate) const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

const DECODE_PEAK_BYTES_PER_PIXEL: u64 = 12;

const DECODE_PEAK_MARGIN: u64 = 16 * 1024 * 1024;

pub(crate) const ICC_PROFILE_LIMIT: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceColor {
    Srgb,
    Converted,
    AssumedSrgb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Bitmap,
    Svg,
}

enum SourceIcc {
    Absent,
    Present(Vec<u8>),
    Unreadable,
}

pub(crate) struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub color: SourceColor,
}

pub(crate) fn decode(
    bytes: &[u8],
    target: Option<(u32, u32)>,
) -> Result<DecodedImage, DecodeCause> {
    let format = format_of(bytes)?;
    let tag = format_tag(format);
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format)
        .into_decoder()
        .map_err(|error| classify(tag, error))?;
    let (width, height) = decoder.dimensions();
    check_pixel_dimensions(tag, width, height)?;
    check_source_size(width, height)?;
    let orientation = decoder
        .orientation()
        .map_err(|error| classify(tag, error))?;
    let icc = match decoder.icc_profile() {
        Ok(Some(profile)) => SourceIcc::Present(profile),
        Ok(None) => SourceIcc::Absent,
        Err(_) => SourceIcc::Unreadable,
    };
    let mut limits = decode_limits();
    limits
        .reserve(decoder.total_bytes())
        .map_err(|error| classify(tag, error))?;
    decoder
        .set_limits(limits)
        .map_err(|error| classify(tag, error))?;
    let mut image: DynamicImage =
        DynamicImage::from_decoder(decoder).map_err(|error| classify(tag, error))?;
    image.apply_orientation(orientation);
    image = resize_decoded(image, target);
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    debug_assert_eq!(rgba.as_raw().len(), width as usize * height as usize * 4);
    let (rgba, color) = convert_icc_to_srgb(icc, rgba.into_raw())?;
    Ok(DecodedImage {
        width,
        height,
        bgra: premultiply(&rgba)?,
        color,
    })
}

pub(crate) fn resolve_target(target: Size, source: (u32, u32)) -> (u32, u32) {
    fn pixel(value: f32) -> u32 {
        if !value.is_finite() || value <= 0.0 {
            1
        } else {
            (value.ceil() as u32).max(1)
        }
    }
    let width = pixel(target.width).min(source.0).min(MAX_TILE_DIM);
    let height = pixel(target.height).min(source.1).min(MAX_TILE_DIM);
    (width, height)
}

fn resize_decoded(image: DynamicImage, target: Option<(u32, u32)>) -> DynamicImage {
    let (width, height) = (image.width(), image.height());
    let (limit, filter) = match target {
        Some(target) => (target, FilterType::CatmullRom),
        None => ((MAX_TILE_DIM, MAX_TILE_DIM), FilterType::Triangle),
    };
    let (final_width, final_height) = fit_into((width, height), limit);
    if (final_width, final_height) == (width, height) {
        return image;
    }
    image.resize_exact(final_width, final_height, filter)
}

fn convert_icc_to_srgb(
    icc: SourceIcc,
    rgba: Vec<u8>,
) -> Result<(Vec<u8>, SourceColor), DecodeCause> {
    let bytes = match icc {
        SourceIcc::Absent => return Ok((rgba, SourceColor::Srgb)),
        SourceIcc::Unreadable => return Ok((rgba, SourceColor::AssumedSrgb)),
        SourceIcc::Present(bytes) => bytes,
    };
    if bytes.len() > ICC_PROFILE_LIMIT {
        return Ok((rgba, SourceColor::AssumedSrgb));
    }
    let Ok(source) = ColorProfile::new_from_slice(&bytes) else {
        return Ok((rgba, SourceColor::AssumedSrgb));
    };
    if is_srgb_profile(&source) {
        return Ok((rgba, SourceColor::Srgb));
    }
    let srgb = ColorProfile::new_srgb();
    let Ok(transform) = source.create_transform_8bit(
        Layout::Rgba,
        &srgb,
        Layout::Rgba,
        TransformOptions::default(),
    ) else {
        return Ok((rgba, SourceColor::AssumedSrgb));
    };
    let mut converted = allocate_zeroed(rgba.len())?;
    if transform.transform(&rgba, &mut converted).is_err() {
        return Ok((rgba, SourceColor::AssumedSrgb));
    }
    Ok((converted, SourceColor::Converted))
}

fn is_srgb_profile(profile: &ColorProfile) -> bool {
    if profile.color_space != DataColorSpace::Rgb || profile.pcs != DataColorSpace::Xyz {
        return false;
    }
    let srgb = ColorProfile::new_srgb();
    let colorants_match = [
        (&profile.red_colorant, &srgb.red_colorant),
        (&profile.green_colorant, &srgb.green_colorant),
        (&profile.blue_colorant, &srgb.blue_colorant),
        (&profile.white_point, &srgb.white_point),
    ]
    .into_iter()
    .all(|(candidate, reference)| xyz_close(candidate, reference));
    colorants_match && trc_matches_srgb(profile, &srgb)
}

fn xyz_close(candidate: &Xyzd, reference: &Xyzd) -> bool {
    const TOLERANCE: f64 = 1e-3;
    (candidate.x - reference.x).abs() < TOLERANCE
        && (candidate.y - reference.y).abs() < TOLERANCE
        && (candidate.z - reference.z).abs() < TOLERANCE
}

const SRGB_TRC_SAMPLES: [f32; 8] = [0.0, 0.05, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];

fn trc_matches_srgb(profile: &ColorProfile, srgb: &ColorProfile) -> bool {
    const TOLERANCE: f32 = 1e-3;
    let channels = [
        (&profile.red_trc, &srgb.red_trc),
        (&profile.green_trc, &srgb.green_trc),
        (&profile.blue_trc, &srgb.blue_trc),
    ];
    channels
        .into_iter()
        .all(|(candidate, reference)| match (candidate, reference) {
            (Some(candidate), Some(reference)) => {
                let (Ok(candidate), Ok(reference)) = (
                    candidate.make_linear_evaluator(),
                    reference.make_linear_evaluator(),
                ) else {
                    return false;
                };
                SRGB_TRC_SAMPLES.into_iter().all(|sample| {
                    (candidate.evaluate_value(sample) - reference.evaluate_value(sample)).abs()
                        < TOLERANCE
                })
            }
            _ => false,
        })
}

pub(crate) fn probe_dimensions(bytes: &[u8]) -> Result<(u32, u32), DecodeCause> {
    let format = format_of(bytes)?;
    let tag = format_tag(format);
    let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|error| classify(tag, error))?;
    check_pixel_dimensions(tag, width, height)?;
    check_source_size(width, height)?;
    Ok((width, height))
}

pub(crate) fn estimate_peak(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * DECODE_PEAK_BYTES_PER_PIXEL + DECODE_PEAK_MARGIN
}

fn decode_limits() -> image::Limits {
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(MAX_SOURCE_DIM);
    limits.max_image_height = Some(MAX_SOURCE_DIM);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

pub(crate) fn check_source_size(width: u32, height: u32) -> Result<(), DecodeCause> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_SOURCE_PIXELS || width > MAX_SOURCE_DIM || height > MAX_SOURCE_DIM {
        return Err(DecodeCause::TooLarge {
            width,
            height,
            pixels,
            limit: MAX_SOURCE_PIXELS,
            dim_limit: MAX_SOURCE_DIM,
        });
    }
    Ok(())
}

fn check_pixel_dimensions(
    format: &'static str,
    width: u32,
    height: u32,
) -> Result<(), DecodeCause> {
    if width == 0 || height == 0 {
        return Err(DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: SmolStr::new_static("image has a zero dimension"),
        });
    }
    Ok(())
}

pub(crate) fn check_source_bytes(len: usize) -> Result<(), DecodeCause> {
    if len > MAX_SOURCE_BYTES {
        return Err(DecodeCause::SourceTooLarge {
            bytes: len as u64,
            limit: MAX_SOURCE_BYTES as u64,
        });
    }
    Ok(())
}

fn format_of(bytes: &[u8]) -> Result<ImageFormat, DecodeCause> {
    let format = image::guess_format(bytes).map_err(|_| DecodeCause::UnsupportedFormat {
        format: SmolStr::new_static("unknown"),
    })?;
    if matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        Ok(format)
    } else {
        Err(DecodeCause::UnsupportedFormat {
            format: SmolStr::new_static(format_tag(format)),
        })
    }
}

fn format_tag(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::WebP => "webp",
        other => other.extensions_str().first().copied().unwrap_or("unknown"),
    }
}

fn classify(format: &'static str, error: ImageError) -> DecodeCause {
    match error {
        ImageError::Limits(inner) => match inner.kind() {
            LimitErrorKind::InsufficientMemory => DecodeCause::OutOfMemory {
                limit_bytes: MAX_DECODE_ALLOC,
            },
            other => DecodeCause::Corrupt {
                format: SmolStr::new_static(format),
                detail: format_smolstr!("decoder limits rejected the image: {other:?}"),
            },
        },
        ImageError::Unsupported(_) => DecodeCause::UnsupportedFormat {
            format: SmolStr::new_static(format),
        },
        ImageError::Decoding(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: format_smolstr!("{inner}"),
        },
        ImageError::Encoding(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: format_smolstr!("{inner}"),
        },
        ImageError::Parameter(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: format_smolstr!("{inner}"),
        },
        ImageError::IoError(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: format_smolstr!("{inner}"),
        },
    }
}

fn fit_into(source: (u32, u32), limit: (u32, u32)) -> (u32, u32) {
    let scale = (limit.0 as f32 / source.0 as f32)
        .min(limit.1 as f32 / source.1 as f32)
        .min(1.0);
    let width = ((source.0 as f32 * scale).floor() as u32).clamp(1, limit.0.max(1));
    let height = ((source.1 as f32 * scale).floor() as u32).clamp(1, limit.1.max(1));
    (width, height)
}

fn premultiply(rgba: &[u8]) -> Result<Vec<u8>, DecodeCause> {
    debug_assert_eq!(rgba.len() % 4, 0, "RGBA8 必须是 4 字节一组");
    let mut out = allocate_zeroed(rgba.len())?;
    for (src, dst) in rgba
        .as_chunks::<4>()
        .0
        .iter()
        .zip(out.as_chunks_mut::<4>().0.iter_mut())
    {
        let alpha = src[3];
        dst[0] = premultiply_channel(src[2], alpha);
        dst[1] = premultiply_channel(src[1], alpha);
        dst[2] = premultiply_channel(src[0], alpha);
        dst[3] = alpha;
    }
    Ok(out)
}

fn allocate_zeroed(len: usize) -> Result<Vec<u8>, DecodeCause> {
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| DecodeCause::OutOfMemory {
            limit_bytes: MAX_DECODE_ALLOC,
        })?;
    out.resize(len, 0);
    Ok(out)
}

fn premultiply_channel(color: u8, alpha: u8) -> u8 {
    match alpha {
        255 => color,
        0 => 0,
        _ => transfer_lut()[usize::from(color) * 256 + usize::from(alpha)],
    }
}

fn transfer_lut() -> &'static [u8; 1 << 16] {
    static LUT: OnceLock<Box<[u8; 1 << 16]>> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut table = Box::new([0u8; 1 << 16]);
        for alpha in 0..=255u8 {
            let factor = f32::from(alpha) / 255.0;
            for color in 0..=255u8 {
                let linear = srgb_decode(color) * factor;
                table[usize::from(color) * 256 + usize::from(alpha)] = srgb_encode(linear);
            }
        }
        table
    })
}

fn srgb_decode(color: u8) -> f32 {
    let encoded = f32::from(color) / 255.0;
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

fn srgb_encode(linear: f32) -> u8 {
    let value = linear.clamp(0.0, 1.0);
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0 + 0.5) as u8
}

#[cfg(test)]
pub(crate) fn test_png(
    width: u32,
    height: u32,
    pixels: &[u8],
    icc: Option<&[u8]>,
    orientation: Option<u16>,
) -> Vec<u8> {
    use image::{ExtendedColorType, ImageEncoder};

    let mut out = Cursor::new(Vec::new());
    let mut encoder = image::codecs::png::PngEncoder::new(&mut out);
    if let Some(icc) = icc {
        encoder
            .set_icc_profile(icc.to_vec())
            .expect("PngEncoder 支持 iCCP");
    }
    if let Some(orientation) = orientation {
        encoder
            .set_exif_metadata(test_exif_orientation(orientation))
            .expect("PngEncoder 支持 eXIf");
    }
    encoder
        .write_image(pixels, width, height, ExtendedColorType::Rgba8)
        .expect("encode png");
    out.into_inner()
}

#[cfg(test)]
fn test_exif_orientation(orientation: u16) -> Vec<u8> {
    let mut exif = Vec::with_capacity(26);
    exif.extend_from_slice(b"II*\0");
    exif.extend_from_slice(&8u32.to_le_bytes());
    exif.extend_from_slice(&1u16.to_le_bytes());
    exif.extend_from_slice(&0x0112u16.to_le_bytes());
    exif.extend_from_slice(&3u16.to_le_bytes());
    exif.extend_from_slice(&1u32.to_le_bytes());
    exif.extend_from_slice(&orientation.to_le_bytes());
    exif.extend_from_slice(&0u16.to_le_bytes());
    exif.extend_from_slice(&0u32.to_le_bytes());
    exif
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::error::LimitErrorKind;
    use moxcms::{ColorProfile, Layout, TransformOptions};

    use super::{
        DECODE_PEAK_MARGIN, ICC_PROFILE_LIMIT, MAX_DECODE_ALLOC, MAX_SOURCE_BYTES, MAX_SOURCE_DIM,
        MAX_SOURCE_PIXELS, MAX_TILE_DIM, SourceColor, SourceIcc, allocate_zeroed,
        check_source_bytes, check_source_size, classify, convert_icc_to_srgb, decode_limits,
        estimate_peak, fit_into, is_srgb_profile, premultiply_channel, resolve_target, srgb_decode,
        srgb_encode, test_png, transfer_lut,
    };
    use crate::{math::Size, resources::DecodeCause};

    fn decode(bytes: &[u8]) -> Result<super::DecodedImage, DecodeCause> {
        super::decode(bytes, None)
    }

    fn png_rgba8(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        test_png(width, height, pixels, None, None)
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width * height {
            pixels.extend_from_slice(&rgba);
        }
        png_rgba8(width, height, &pixels)
    }

    fn png_with_icc(width: u32, height: u32, pixels: &[u8], icc: &[u8]) -> Vec<u8> {
        test_png(width, height, pixels, Some(icc), None)
    }

    fn png_with_exif(width: u32, height: u32, pixels: &[u8], orientation: u16) -> Vec<u8> {
        test_png(width, height, pixels, None, Some(orientation))
    }

    fn display_p3_icc() -> Vec<u8> {
        ColorProfile::new_display_p3()
            .encode()
            .expect("encode display p3 profile")
    }

    fn srgb_icc() -> Vec<u8> {
        ColorProfile::new_srgb()
            .encode()
            .expect("encode srgb profile")
    }

    fn transform_rgba(icc: &[u8], pixel: [u8; 4]) -> [u8; 4] {
        let source = ColorProfile::new_from_slice(icc).expect("valid profile");
        let srgb = ColorProfile::new_srgb();
        let transform = source
            .create_transform_8bit(
                Layout::Rgba,
                &srgb,
                Layout::Rgba,
                TransformOptions::default(),
            )
            .expect("create transform");
        let mut out = [0u8; 4];
        transform.transform(&pixel, &mut out).expect("transform");
        out
    }

    fn cause(bytes: &[u8]) -> DecodeCause {
        match decode(bytes) {
            Err(cause) => cause,
            Ok(_) => panic!("expected a decode failure"),
        }
    }

    #[test]
    fn decode_opaque_png_is_byte_exact_in_bgra() {
        let png = solid(2, 1, [10, 20, 30, 255]);
        let image = decode(&png).expect("png");

        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(
            &image.bgra[..4],
            &[30, 20, 10, 255],
            "BGRA 顺序且不透明像素不得被量化改动"
        );
        assert_eq!(&image.bgra[4..8], &image.bgra[..4]);
    }

    #[test]
    fn decode_premultiplies_in_linear_space() {
        let png = solid(1, 1, [128, 128, 128, 128]);
        let image = decode(&png).expect("png");

        assert_eq!(&image.bgra[..4], &[93, 93, 93, 128]);
        assert!(
            image.bgra[0] > 64,
            "编码域预乘会偏暗到 64：{}",
            image.bgra[0]
        );
    }

    #[test]
    fn decode_alpha_zero_zeroes_rgb() {
        let png = solid(1, 1, [200, 100, 50, 0]);
        let image = decode(&png).expect("png");

        assert_eq!(&image.bgra[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn decode_swaps_red_and_blue() {
        let png = solid(1, 1, [255, 0, 0, 255]);
        let image = decode(&png).expect("png");
        assert_eq!(
            &image.bgra[..4],
            &[0, 0, 255, 255],
            "纯红必须落在 BGR 的最后一个分量"
        );

        let png = solid(1, 1, [0, 0, 255, 255]);
        let image = decode(&png).expect("png");
        assert_eq!(
            &image.bgra[..4],
            &[255, 0, 0, 255],
            "纯蓝必须落在第一个分量"
        );
    }

    #[test]
    fn decode_16bit_downshifts_to_8bit() {
        let mut wide: Vec<u16> = Vec::with_capacity(4 * 4);
        for value in [0u16, 16_000, 32_000, 48_000] {
            wide.extend_from_slice(&[value, value, value, u16::MAX]);
        }
        let image16 = image::ImageBuffer::<image::Rgba<u16>, _>::from_raw(2, 2, wide).expect("raw");
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba16(image16)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode wide png");
        let decoded = decode(out.get_ref()).expect("16-bit png");

        let reds: Vec<u8> = decoded
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[2])
            .collect();
        assert_eq!(reds, vec![0, 62, 125, 187], "to_rgba8 的降位结果必须稳定");
    }

    #[test]
    fn decode_downsamples_without_upscaling() {
        let big = solid(4000, 100, [1, 2, 3, 255]);
        let image = decode(&big).expect("big png");
        assert_eq!((image.width, image.height), (MAX_TILE_DIM, 51));

        let small = solid(3, 2, [9, 9, 9, 255]);
        let image = decode(&small).expect("small png");
        assert_eq!((image.width, image.height), (3, 2));
    }

    #[test]
    fn decode_never_returns_zero_dimension() {
        let one = solid(1, 1, [1, 2, 3, 255]);
        assert_eq!(decode(&one).expect("1x1").width, 1);

        let strip = solid(4000, 1, [1, 2, 3, 255]);
        let image = decode(&strip).expect("strip png");
        assert_eq!((image.width, image.height), (MAX_TILE_DIM, 1));
    }

    #[test]
    fn decode_applies_exif_orientation_before_scaling() {
        let red_blue = [255u8, 0, 0, 255, 0, 0, 255, 255];

        let rotated = decode(&png_with_exif(2, 1, &red_blue, 6)).expect("rotate90 png");
        assert_eq!(
            (rotated.width, rotated.height),
            (1, 2),
            "EXIF 6 必须交换宽高"
        );
        assert_eq!(
            rotated.bgra.as_slice(),
            &[0, 0, 255, 255, 255, 0, 0, 255],
            "EXIF 6 的像素顺序：上红下蓝"
        );

        let flipped = decode(&png_with_exif(2, 1, &red_blue, 2)).expect("fliph png");
        assert_eq!((flipped.width, flipped.height), (2, 1), "EXIF 2 不改尺寸");
        assert_eq!(
            flipped.bgra.as_slice(),
            &[255, 0, 0, 255, 0, 0, 255, 255],
            "EXIF 2 的像素顺序：左蓝右红"
        );
    }

    #[test]
    fn decode_without_orientation_transforms_is_byte_exact() {
        let baseline = decode(&solid(2, 1, [10, 20, 30, 255])).expect("plain png");

        let pixels = [10u8, 20, 30, 255, 10, 20, 30, 255];
        let tagged = decode(&png_with_exif(2, 1, &pixels, 1)).expect("orientation 1");
        assert_eq!(
            (tagged.width, tagged.height),
            (baseline.width, baseline.height),
            "orientation 1 不得改变尺寸"
        );
        assert_eq!(
            tagged.bgra, baseline.bgra,
            "orientation 1 必须与无 eXIf 的像素逐字节一致"
        );
    }

    #[test]
    fn decode_converts_display_p3_to_srgb() {
        let icc = display_p3_icc();
        let p3_red = [234u8, 51, 35, 255];
        let image = decode(&png_with_icc(1, 1, &p3_red, &icc)).expect("P3 png");

        assert_eq!(image.color, SourceColor::Converted, "P3 必须走转换路径");
        assert_eq!((image.width, image.height), (1, 1));
        let expected = transform_rgba(&icc, p3_red);
        assert_eq!(
            image.bgra.as_slice(),
            &[expected[2], expected[1], expected[0], expected[3]],
            "解码管线的 ICC 结果必须与 moxcms 直接转换逐字节一致（BGRA 顺序）"
        );
        assert!(
            image.bgra[0] <= 2 && image.bgra[1] <= 2 && image.bgra[2] >= 250,
            "P3 编码下的 sRGB 红应转回 sRGB 红，实际 {:?}",
            &image.bgra[..4]
        );

        let untagged = decode(&png_rgba8(1, 1, &p3_red)).expect("untagged png");
        assert_eq!(untagged.color, SourceColor::Srgb);
        assert_ne!(
            image.bgra, untagged.bgra,
            "带 P3 profile 与无 profile 的像素必须不同"
        );
    }

    #[test]
    fn decode_srgb_icc_is_byte_exact() {
        let icc = srgb_icc();
        let profile = ColorProfile::new_from_slice(&icc).expect("sRGB profile 可解析");
        assert!(
            is_srgb_profile(&profile),
            "moxcms 编码的 sRGB profile 必须被识别为 sRGB"
        );
        let p3 = ColorProfile::new_from_slice(&display_p3_icc()).expect("P3 profile 可解析");
        assert!(
            !is_srgb_profile(&p3),
            "Display P3 不是 sRGB（primaries 不同）"
        );

        let pixels = [
            255u8, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 128, 64, 32, 128, 10, 20, 30, 0, 1,
            2, 3, 255,
        ];
        let plain = decode(&png_rgba8(6, 1, &pixels)).expect("plain png");
        let tagged = decode(&png_with_icc(6, 1, &pixels, &icc)).expect("sRGB icc png");
        assert_eq!(tagged.color, SourceColor::Srgb);
        assert_eq!(
            tagged.bgra, plain.bgra,
            "sRGB profile 必须短路：像素与无 profile 路径逐字节相同"
        );
    }

    #[test]
    fn decode_icc_without_profile_keeps_bytes_exact() {
        let pixels = [10u8, 20, 30, 128, 200, 100, 50, 255];
        let plain = decode(&png_rgba8(2, 1, &pixels)).expect("plain png");
        assert_eq!(plain.color, SourceColor::Srgb);
        let tagged = decode(&png_with_exif(2, 1, &pixels, 1)).expect("orientation 1");
        assert_eq!(plain.bgra, tagged.bgra);
    }

    #[test]
    fn decode_invalid_icc_falls_back_to_srgb() {
        let pixels = [12u8, 34, 56, 128, 78, 90, 12, 255];
        let plain = decode(&png_rgba8(2, 1, &pixels)).expect("plain png");
        let gray_icc = ColorProfile::new_gray_with_gamma(2.2)
            .encode()
            .expect("encode gray profile");
        let cases: [(&str, Vec<u8>); 3] = [
            ("垃圾字节", b"not an icc profile".to_vec()),
            ("超过 ICC_PROFILE_LIMIT", vec![0u8; ICC_PROFILE_LIMIT + 1]),
            ("Gray profile（RGBA 布局不支持）", gray_icc),
        ];
        for (label, icc) in cases {
            let tagged = decode(&png_with_icc(2, 1, &pixels, &icc))
                .unwrap_or_else(|error| panic!("{label}: 坏 ICC 不得使解码失败：{error:?}"));
            assert_eq!(tagged.color, SourceColor::AssumedSrgb, "{label}");
            assert_eq!(
                tagged.bgra, plain.bgra,
                "{label}: 降级路径的像素必须等于无 profile 路径"
            );
        }
    }

    #[test]
    fn decode_applies_orientation_before_icc_and_premultiplies_after() {
        let icc = display_p3_icc();
        let pixels = [234u8, 51, 35, 255, 0, 0, 0, 255];
        let rotated =
            decode(&test_png(2, 1, &pixels, Some(&icc), Some(6))).expect("rotated P3 png");
        assert_eq!((rotated.width, rotated.height), (1, 2), "EXIF 6 先转置尺寸");

        let expected = transform_rgba(&icc, [234, 51, 35, 255]);
        assert_eq!(
            &rotated.bgra[..4],
            &[expected[2], expected[1], expected[0], expected[3]],
            "上像素：orientation 之后再做 ICC"
        );
        assert_eq!(&rotated.bgra[4..], &[0, 0, 0, 255], "黑色转换后仍是黑");
        assert_eq!(rotated.color, SourceColor::Converted);
    }

    #[test]
    fn icc_conversion_is_zero_copy_when_no_work_is_needed() {
        let cases: [(&str, SourceIcc, SourceColor); 4] = [
            ("absent", SourceIcc::Absent, SourceColor::Srgb),
            ("srgb", SourceIcc::Present(srgb_icc()), SourceColor::Srgb),
            (
                "fallback",
                SourceIcc::Present(b"not an icc profile".to_vec()),
                SourceColor::AssumedSrgb,
            ),
            (
                "oversize",
                SourceIcc::Present(vec![0u8; ICC_PROFILE_LIMIT + 1]),
                SourceColor::AssumedSrgb,
            ),
        ];
        for (label, icc, expected) in cases {
            let rgba = vec![7u8; 64];
            let pointer = rgba.as_ptr();
            let (converted, color) =
                convert_icc_to_srgb(icc, rgba).expect("ICC 降级不产生解码失败");
            assert_eq!(converted.as_ptr(), pointer, "{label}: 不需要转换时不得拷贝");
            assert_eq!(converted, vec![7u8; 64], "{label}: 像素必须原样返回");
            assert_eq!(color, expected, "{label}");
        }
    }

    #[test]
    fn fit_into_preserves_aspect_and_never_upscales() {
        let tile = (MAX_TILE_DIM, MAX_TILE_DIM);
        assert_eq!(fit_into((10, 10), tile), (10, 10));
        assert_eq!(fit_into((4092, 2046), tile), (MAX_TILE_DIM, 1023));
        assert_eq!(fit_into((1, 100_000), tile), (1, MAX_TILE_DIM));
        assert_eq!(fit_into((200, 100), (40, 40)), (40, 20));
        assert_eq!(fit_into((200, 100), (300, 40)), (80, 40));
        assert_eq!(fit_into((40, 20), (200, 200)), (40, 20));
        assert_eq!(fit_into((600, 300), (40, 40)), (40, 20));
    }

    #[test]
    fn resolve_target_rounds_up_and_clamps_to_source_and_tile() {
        assert_eq!(resolve_target(Size::new(40.2, 20.1), (200, 100)), (41, 21));
        assert_eq!(resolve_target(Size::new(40.0, 20.0), (200, 100)), (40, 20));
        assert_eq!(resolve_target(Size::new(200.0, 200.0), (40, 20)), (40, 20));
        assert_eq!(resolve_target(Size::new(500.0, 500.0), (40, 20)), (40, 20));
        assert_eq!(
            resolve_target(Size::new(4000.0, 4000.0), (5000, 5000)),
            (MAX_TILE_DIM, MAX_TILE_DIM)
        );
        assert_eq!(resolve_target(Size::new(0.0, -3.0), (200, 100)), (1, 1));
        assert_eq!(
            resolve_target(Size::new(f32::NAN, f32::INFINITY), (200, 100)),
            (1, 1)
        );
    }

    #[test]
    fn decode_at_size_keeps_aspect_and_never_upscales() {
        let bytes = solid(200, 100, [10, 20, 30, 255]);
        let sized = super::decode(&bytes, Some((40, 40))).expect("sized decode");
        assert_eq!((sized.width, sized.height), (40, 20), "等比 fit，不是拉伸");

        let small = solid(40, 20, [1, 2, 3, 255]);
        let not_upscaled = super::decode(&small, Some((200, 200))).expect("no upscale");
        assert_eq!(
            (not_upscaled.width, not_upscaled.height),
            (40, 20),
            "目标大于源尺寸时不得放大"
        );
        assert_eq!(
            not_upscaled.bgra,
            decode(&small).expect("原尺寸").bgra,
            "不放大时像素与原尺寸路径逐字节一致"
        );
    }

    #[test]
    fn decode_rejects_unsupported_formats() {
        assert_eq!(
            cause(b"GIF89a\x01\x00\x01\x00\x00\x00\x00"),
            DecodeCause::UnsupportedFormat {
                format: smol_str::SmolStr::new_static("gif")
            }
        );
        assert_eq!(
            cause(&[0u8; 8]),
            DecodeCause::UnsupportedFormat {
                format: smol_str::SmolStr::new_static("unknown")
            }
        );
    }

    #[test]
    fn decode_empty_input_is_unknown_format() {
        assert_eq!(
            cause(&[]),
            DecodeCause::UnsupportedFormat {
                format: smol_str::SmolStr::new_static("unknown")
            }
        );
    }

    #[test]
    fn decode_rejects_oversize_source() {
        let wide = solid(MAX_SOURCE_DIM + 1, 1, [1, 2, 3, 255]);
        assert_eq!(
            cause(&wide),
            DecodeCause::TooLarge {
                width: MAX_SOURCE_DIM + 1,
                height: 1,
                pixels: u64::from(MAX_SOURCE_DIM) + 1,
                limit: MAX_SOURCE_PIXELS,
                dim_limit: MAX_SOURCE_DIM,
            }
        );
    }

    #[test]
    fn source_size_predicate_covers_pixels_and_single_sides() {
        assert!(check_source_size(1, 1).is_ok());
        assert!(check_source_size(4096, 4096).is_ok(), "16 M px 正好是上限");
        assert!(
            check_source_size(8192, 1).is_ok(),
            "8192×1 = 8192 px：单边合法、像素数远低于上限"
        );

        assert_eq!(
            check_source_size(4097, 4097),
            Err(DecodeCause::TooLarge {
                width: 4097,
                height: 4097,
                pixels: 4097 * 4097,
                limit: MAX_SOURCE_PIXELS,
                dim_limit: MAX_SOURCE_DIM,
            })
        );
        assert!(check_source_size(MAX_SOURCE_DIM + 1, 1).is_err());
        assert!(check_source_size(1, MAX_SOURCE_DIM + 1).is_err());
        assert_eq!(MAX_SOURCE_PIXELS, 16 * 1024 * 1024, "像素上限 16 M px");
    }

    #[test]
    fn source_bytes_predicate_rejects_oversize_input() {
        assert!(check_source_bytes(0).is_ok());
        assert!(check_source_bytes(MAX_SOURCE_BYTES).is_ok());
        assert_eq!(
            check_source_bytes(MAX_SOURCE_BYTES + 1),
            Err(DecodeCause::SourceTooLarge {
                bytes: MAX_SOURCE_BYTES as u64 + 1,
                limit: MAX_SOURCE_BYTES as u64,
            })
        );
    }

    #[test]
    fn decode_limits_cap_memory_and_single_sides() {
        let limits = decode_limits();
        assert_eq!(limits.max_alloc, Some(MAX_DECODE_ALLOC));
        assert_eq!(limits.max_image_width, Some(MAX_SOURCE_DIM));
        assert_eq!(limits.max_image_height, Some(MAX_SOURCE_DIM));
        assert_eq!(MAX_DECODE_ALLOC, 256 * 1024 * 1024);
        assert!(
            MAX_DECODE_ALLOC < image::Limits::default().max_alloc.expect("默认上限"),
            "必须比 image 的默认 512 MiB 更严"
        );
    }

    #[test]
    fn decode_peak_estimate_is_bounded_by_max_decode_alloc() {
        assert_eq!(estimate_peak(1, 1), 12 + DECODE_PEAK_MARGIN);
        assert_eq!(
            estimate_peak(200, 100),
            200 * 100 * 12 + DECODE_PEAK_MARGIN,
            "估算按文件头像素数线性增长（RGBA16 + RGBA8 + ICC dst = 12 B/px）"
        );
        assert!(
            estimate_peak(4096, 4096) <= MAX_DECODE_ALLOC,
            "16 M px 合法图的估算上界必须仍受默认预算接纳：{}",
            estimate_peak(4096, 4096)
        );
    }

    #[test]
    fn try_reserve_failure_maps_to_out_of_memory() {
        let buffer = allocate_zeroed(16).expect("小缓冲分配");
        assert_eq!(buffer, vec![0u8; 16], "缓冲必须零初始化（预乘逐像素覆盖）");

        assert_eq!(
            allocate_zeroed(usize::MAX),
            Err(DecodeCause::OutOfMemory {
                limit_bytes: MAX_DECODE_ALLOC
            }),
            "容量溢出必须走错误路径而不是 abort"
        );
    }

    #[test]
    fn classify_distinguishes_decoder_memory_from_corruption() {
        let out_of_memory = classify(
            "png",
            image::ImageError::Limits(LimitErrorKind::InsufficientMemory.into()),
        );
        assert_eq!(
            out_of_memory,
            DecodeCause::OutOfMemory {
                limit_bytes: MAX_DECODE_ALLOC,
            }
        );
        let dimension = classify(
            "png",
            image::ImageError::Limits(LimitErrorKind::DimensionError.into()),
        );
        assert!(
            matches!(&dimension, DecodeCause::Corrupt { format, .. } if format == "png"),
            "{dimension:?}"
        );
    }

    #[test]
    fn decode_maps_corrupt_bytes() {
        let png = solid(4, 4, [1, 2, 3, 255]);
        let truncated = &png[..png.len() / 2];
        assert!(
            matches!(cause(truncated), DecodeCause::Corrupt { format, .. } if format == "png"),
            "截断的 PNG 必须映射成 Corrupt(png)"
        );
    }

    #[test]
    fn decode_jpeg_and_webp() {
        let rgba = image::RgbaImage::from_pixel(8, 6, image::Rgba([200, 120, 40, 255]));
        for format in [image::ImageFormat::Jpeg, image::ImageFormat::WebP] {
            let mut out = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(rgba.clone())
                .write_to(&mut out, format)
                .expect("encode");
            let image = decode(out.get_ref()).expect("decode supported format");
            assert_eq!((image.width, image.height), (8, 6));
            assert_eq!(image.bgra.len(), 8 * 6 * 4);
        }
    }

    #[test]
    fn premultiply_known_values() {
        assert_eq!(premultiply_channel(255, 255), 255);
        assert_eq!(premultiply_channel(0, 128), 0);
        assert_eq!(premultiply_channel(128, 0), 0);
        assert_eq!(premultiply_channel(128, 255), 128);
        assert_eq!(premultiply_channel(128, 128), 93);
        assert_eq!(premultiply_channel(255, 128), 188);
        assert_eq!(premultiply_channel(16, 128), 9);
    }

    #[test]
    fn transfer_lut_matches_scalar_formula_for_every_pair() {
        let lut = transfer_lut();
        for alpha in 0..=255u8 {
            let factor = f32::from(alpha) / 255.0;
            for color in 0..=255u8 {
                let expected = srgb_encode(srgb_decode(color) * factor);
                assert_eq!(
                    lut[usize::from(color) * 256 + usize::from(alpha)],
                    expected,
                    "color {color} alpha {alpha}"
                );
            }
        }
    }

    #[test]
    fn premultiply_fast_paths_are_byte_exact() {
        for color in 0..=255u8 {
            assert_eq!(
                premultiply_channel(color, 255),
                color,
                "opaque color {color}"
            );
            assert_eq!(
                premultiply_channel(color, 0),
                0,
                "transparent color {color}"
            );
        }
    }

    #[test]
    fn srgb_round_trip_is_stable_for_8bit() {
        for value in 0..=255u8 {
            let encoded = srgb_encode(srgb_decode(value));
            assert!(
                encoded.abs_diff(value) <= 1,
                "srgb 往返 8 bit 抖动 > 1：{value} → {encoded}"
            );
        }
    }
}
