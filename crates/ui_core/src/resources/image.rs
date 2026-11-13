//! 图片解码：字节 → **线性空间预乘 → sRGB 重编码**的 `BGRA8`（P13 §6）。
//!
//! 图集字节语义由 `render/shaders/contract.md` §8 冻结（纹素是 linear-premultiplied 后 sRGB 编码），
//! 本模块就是它的 CPU 侧实现。三个约束：
//!
//! 1. 只接受 PNG / JPEG / WebP（`image::guess_format` 与 feature 无关，所以必须**策略性**拒绝其它格式）；
//! 2. 不做色彩管理：`image 0.25` 把 ICC 当元数据透传，资产必须**已经是 sRGB 编码**；
//! 3. 大图在解码后按 `MAX_TILE_DIM` 等比缩小（编码域 `Triangle`，与浏览器一致），不做线性空间重采样。
//!
//! 预乘在**线性**空间完成：`srgb_decode(rgb) * alpha` 再 `srgb_encode` 存 8 bit。直接在编码域乘 alpha
//! 在中灰半透明处会偏暗最多 60/255（probe 实测），因此用 64 KiB 的 `(color, alpha) → stored` 查找表，
//! 每像素 3 次查表；`alpha == 255` / `alpha == 0` 走快路径，保证不透明资产**逐字节不变**。

use std::{io::Cursor, sync::OnceLock};

use image::{
    DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, imageops::FilterType,
};
use smol_str::SmolStr;

use crate::resources::DecodeCause;

/// 图片页能容纳的单块上限（2048 − 2 纹素 padding，P02 §10.3）。
pub(crate) const MAX_TILE_DIM: u32 = 2046;

/// 源图单边上限。再大属解压炸弹/资产错误，应用层应预缩放。
pub(crate) const MAX_SOURCE_DIM: u32 = 8192;

/// 解码结果：**预乘 sRGB 编码**的 BGRA8（行主序，行宽 `width * 4`）。
pub(crate) struct DecodedImage {
    /// 解码（必要时缩放）后的像素宽。
    pub width: u32,
    /// 解码（必要时缩放）后的像素高。
    pub height: u32,
    /// `width * height * 4` 字节，BGR(A) 顺序。
    pub bgra: Box<[u8]>,
}

/// 字节 → BGRA8 预乘 sRGB。
///
/// # Errors
/// [`DecodeCause`]：格式不支持 / 字节损坏 / 源图过大（四类里除 `Capacity` 外都可能出现）。
pub(crate) fn decode(bytes: &[u8]) -> Result<DecodedImage, DecodeCause> {
    let format = format_of(bytes)?;
    let tag = format_tag(format);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    // 只保留默认的 `max_alloc`（512 MiB）防炸弹；尺寸上限由我们自己判，才能报出精确的 `width/height`。
    reader.limits(image::Limits::default());
    let decoder = reader
        .into_decoder()
        .map_err(|error| classify(tag, error))?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err(DecodeCause::Corrupt {
            format: SmolStr::new_static(tag),
            detail: SmolStr::new_static("image has a zero dimension"),
        });
    }
    if width > MAX_SOURCE_DIM || height > MAX_SOURCE_DIM {
        return Err(DecodeCause::TooLarge {
            width,
            height,
            limit: MAX_SOURCE_DIM,
        });
    }
    let mut image = DynamicImage::from_decoder(decoder).map_err(|error| classify(tag, error))?;
    if image.width() > MAX_TILE_DIM || image.height() > MAX_TILE_DIM {
        let (target_width, target_height) = fit_within(image.width(), image.height());
        image = image.resize_exact(target_width, target_height, FilterType::Triangle);
    }
    let rgba = image.to_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    debug_assert_eq!(rgba.as_raw().len(), width as usize * height as usize * 4);
    Ok(DecodedImage {
        width,
        height,
        bgra: premultiply(rgba.as_raw()),
    })
}

/// 格式判定 + 策略：只放行 PNG / JPEG / WebP。
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

/// 诊断用的稳定格式名（只给三种受支持格式 + 其它格式的首个扩展名）。
fn format_tag(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::WebP => "webp",
        other => other.extensions_str().first().copied().unwrap_or("unknown"),
    }
}

/// `image` 的错误 → 强类型分类。穷尽 match：`image` 升级新增变体会编译失败（期望行为）。
fn classify(format: &'static str, error: ImageError) -> DecodeCause {
    match error {
        // 走到这里的 `Limits` 只可能是解码器内存上限（尺寸上限已被我们自己拦下）。
        ImageError::Limits(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: SmolStr::new(format!("decoder limits exceeded: {inner}")),
        },
        ImageError::Unsupported(_) => DecodeCause::UnsupportedFormat {
            format: SmolStr::new_static(format),
        },
        ImageError::Decoding(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: SmolStr::new(inner.to_string()),
        },
        ImageError::Encoding(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: SmolStr::new(inner.to_string()),
        },
        ImageError::Parameter(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: SmolStr::new(inner.to_string()),
        },
        ImageError::IoError(inner) => DecodeCause::Corrupt {
            format: SmolStr::new_static(format),
            detail: SmolStr::new(inner.to_string()),
        },
    }
}

/// 等比缩到 `MAX_TILE_DIM` 以内（不放大；两边都至少 1）。
fn fit_within(width: u32, height: u32) -> (u32, u32) {
    let limit = MAX_TILE_DIM as f32;
    let scale = (limit / width as f32).min(limit / height as f32).min(1.0);
    let target_width = ((width as f32 * scale).floor() as u32).clamp(1, MAX_TILE_DIM);
    let target_height = ((height as f32 * scale).floor() as u32).clamp(1, MAX_TILE_DIM);
    (target_width, target_height)
}

/// RGBA8 → 预乘 sRGB 编码的 BGRA8。
fn premultiply(rgba: &[u8]) -> Box<[u8]> {
    debug_assert_eq!(rgba.len() % 4, 0, "RGBA8 必须是 4 字节一组");
    let mut out = vec![0u8; rgba.len()].into_boxed_slice();
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
    out
}

/// 单通道预乘：`srgb_encode(srgb_decode(color) * alpha)`。
fn premultiply_channel(color: u8, alpha: u8) -> u8 {
    match alpha {
        // 快路径：不透明资产逐字节不变（LUT 的 8 bit 往返会有 ±1 的量化抖动）。
        255 => color,
        0 => 0,
        _ => transfer_lut()[usize::from(color) * 256 + usize::from(alpha)],
    }
}

/// 64 KiB 查找表：键 `(color << 8) | alpha`。
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

/// sRGB 解码（IEC 61966-2-1 的分段函数）。
fn srgb_decode(color: u8) -> f32 {
    let encoded = f32::from(color) / 255.0;
    if encoded <= 0.040_45 {
        encoded / 12.92
    } else {
        ((encoded + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB 编码（`srgb_decode` 的逆，含四舍五入）。
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
mod tests {
    use std::io::Cursor;

    use super::{
        MAX_SOURCE_DIM, MAX_TILE_DIM, decode, fit_within, premultiply_channel, srgb_decode,
        srgb_encode, transfer_lut,
    };
    use crate::resources::DecodeCause;

    fn png_rgba8(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let image = image::RgbaImage::from_raw(width, height, pixels.to_vec()).expect("raw");
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
        for _ in 0..width * height {
            pixels.extend_from_slice(&rgba);
        }
        png_rgba8(width, height, &pixels)
    }

    fn cause(bytes: &[u8]) -> DecodeCause {
        match decode(bytes) {
            Err(cause) => cause,
            Ok(_) => panic!("expected a decode failure"),
        }
    }

    #[test]
    fn decode_opaque_png_is_byte_exact_in_bgra() {
        // a = 255 ⇒ 预乘是恒等；只剩 sRGB 编码域不变 + B/R 交换。
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
        // probe 值：中灰 128 @ alpha 128 → 存储 93（编码域预乘只会得到 64）。
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

        // 小图不放大。
        let small = solid(3, 2, [9, 9, 9, 255]);
        let image = decode(&small).expect("small png");
        assert_eq!((image.width, image.height), (3, 2));
    }

    #[test]
    fn decode_never_returns_zero_dimension() {
        let one = solid(1, 1, [1, 2, 3, 255]);
        assert_eq!(decode(&one).expect("1x1").width, 1);

        // 4000×1：等比缩放后高度会 floor 到 0，必须 clamp 到 1。
        let strip = solid(4000, 1, [1, 2, 3, 255]);
        let image = decode(&strip).expect("strip png");
        assert_eq!((image.width, image.height), (MAX_TILE_DIM, 1));
    }

    #[test]
    fn fit_within_preserves_aspect_and_never_upscales() {
        assert_eq!(fit_within(10, 10), (10, 10));
        assert_eq!(fit_within(4092, 2046), (MAX_TILE_DIM, 1023));
        assert_eq!(fit_within(1, 100_000), (1, MAX_TILE_DIM));
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
                limit: MAX_SOURCE_DIM
            }
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
        // 端点与两个 probe 实测值（独立于 LUT 实现的手算结果）。
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
        // 全 256×256 组合：LUT 必须与逐像素标量公式**逐位**一致（含 alpha = 0/255 的边角）。
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
        // alpha = 255 必须逐字节不变（不透明资产不能被 LUT 量化抖动）；alpha = 0 必须全 0。
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
