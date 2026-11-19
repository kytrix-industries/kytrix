//! 图片解码：字节 → **色彩管理 → 线性空间预乘 → sRGB 重编码**的 `BGRA8`（P13 §6，D-8）。
//!
//! 图集字节语义由 `render/shaders/contract.md` §8 冻结（纹素是 linear-premultiplied 后 sRGB 编码），
//! 本模块就是它的 CPU 侧实现。五条约束：
//!
//! 1. 只接受 PNG / JPEG / WebP（`image::guess_format` 与 feature 无关，所以必须**策略性**拒绝其它格式）；
//! 2. 色彩管理（D-8）：带 ICC profile 的资产在解码期用 `moxcms` 转换到 sRGB（引擎 RT 是 sRGB 编码，
//!    合成假设 sRGB；`paint/color.rs` 的 DisplayP3 只用于主题纯色，图片管线没有宽色域通道）。
//!    只做 **8-bit ICC → sRGB**；**不做 gAMA / cHRM**（`image` 0.25.10 没有 cHRM 访问器，gAMA 只有
//!    `PngDecoder::gamma_value()` 这个 inherent 方法，`into_decoder()` 拿不到；见阶段 1 审计 D-8 决策 3）；
//! 3. EXIF orientation 在缩放 / 转 RGBA / ICC / 预乘**之前**应用到像素（`ImageDecoder::orientation` +
//!    `DynamicImage::apply_orientation`）；cache key 仍是源字节，方向不进键；
//! 4. 大图在解码后按 `MAX_TILE_DIM` 等比缩小（编码域 `Triangle`，与浏览器一致），不做线性空间重采样；
//! 5. 像素顺序固定为：元数据 → 解码 → orientation → 缩放 → `to_rgba8` → ICC → 预乘。
//!
//! 解码流程（[`decode`]）固定为：`format_of` → `into_decoder()` → `dimensions` +
//! [`check_source_size`] → 元数据（orientation + ICC，**必须先于** `from_decoder`：解码器一旦被消耗，
//! 就再没有 `icc_profile()` 的机会）→ `Limits::reserve(decoder.total_bytes())` + `set_limits` →
//! `DynamicImage::from_decoder` → `apply_orientation` → 缩放 → `to_rgba8` → **ICC 转换到 sRGB** → 预乘。
//! `reserve` 是复刻 `ImageReader::decode` 的「整图分配」预检：走 `into_decoder` 而漏掉它，
//! [`MAX_DECODE_ALLOC`] 就只剩解码器中间缓冲有意义。
//!
//! ICC 策略（[`convert_icc_to_srgb`]）：
//!
//! - **无 ICC** ⇒ 现状路径，逐字节不变（性能护栏：不分配、不拷贝）；
//! - **可解析且是 sRGB**（primaries + white point + TRC 数值近似，[`is_srgb_profile`]）⇒ **短路**，
//!   逐字节不变。不能依赖「sRGB→sRGB transform 恒等」：8-bit 定点管线有量化抖动；
//! - **可解析的非 sRGB profile** ⇒ `create_transform_8bit(Rgba → sRGB)` 在 `to_rgba8()` 的 RGBA8 上
//!   转换一次（src/dst 分离，dst 走 `try_reserve_exact`）；转换失败也按 sRGB 降级（**不**让整图失败）；
//! - **解析失败 / 不支持（Gray/CMYK/DeviceLink…）/ 超过 [`ICC_PROFILE_LIMIT`] / 读取失败** ⇒ 按 sRGB
//!   继续（像素 = 无 profile 路径），由 [`SourceColor::AssumedSrgb`] 计数（诊断面）。
//!
//! 分配失败：本模块自己的预乘 / ICC dst 缓冲走 `try_reserve_exact` ⇒ [`DecodeCause::OutOfMemory`]；
//! `image` 内部缓冲仍是 `vec!`（分配失败 = abort），预算准入（[`estimate_peak`] + `ResourceCache`
//! 的 byte budget）把单张峰值压在 [`MAX_DECODE_ALLOC`] 内，残余 abort 风险只留给绕过预算的调用方。
//!
//! 预乘在**线性**空间完成：`srgb_decode(rgb) * alpha` 再 `srgb_encode` 存 8 bit。直接在编码域乘 alpha
//! 在中灰半透明处会偏暗最多 60/255（probe 实测），因此用 64 KiB 的 `(color, alpha) → stored` 查找表，
//! 每像素 3 次查表；`alpha == 255` / `alpha == 0` 走快路径，保证不透明资产**逐字节不变**。

use std::{io::Cursor, sync::OnceLock};

use image::{
    DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, error::LimitErrorKind,
    imageops::FilterType,
};
use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions, Xyzd};
use smol_str::{SmolStr, format_smolstr};

use crate::resources::DecodeCause;

/// 图片页能容纳的单块上限（2048 − 2 纹素 padding，P02 §10.3）。
pub(crate) const MAX_TILE_DIM: u32 = 2046;

/// 源图单边上限。再大属解压炸弹/资产错误，应用层应预缩放。
pub(crate) const MAX_SOURCE_DIM: u32 = 8192;

/// 源图像素上限（R12-15：判据从「单边 8192」改成像素数）。
///
/// 16 M px ≈ 64 MiB RGBA8 / 128 MiB RGBA16 的峰值，一张 4096×4096 的图正好卡满；
/// 单边判据（[`MAX_SOURCE_DIM`]）只作为解码器 `Limits` 的纵深防御保留。
pub(crate) const MAX_SOURCE_PIXELS: u64 = 4096 * 4096;

/// 输入字节上限（`insert` / `insert_async` 的**轻量**校验：UI 线程不做格式嗅探，但不会为
/// 荒谬大的缓冲投递解码任务）。
///
/// 取 [`MAX_SOURCE_PIXELS`] 的 RGBA8 解码尺寸（16 M px × 4 B = 64 MiB）：任何能通过像素判据的
/// 合法 PNG / JPEG / WebP 压缩字节都不会超过它，因此这条上限只拦「明显不是图片 / 解压炸弹」的输入。
pub(crate) const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;

/// 解码器的瞬时分配上限（`image::Limits::max_alloc`）。
///
/// `image` 的默认值是 512 MiB：一张 8192×8192 的 RGBA16 PNG 正好卡在那里，会以
/// `Corrupt`（而非「内存不足」）失败。256 MiB 让 [`MAX_SOURCE_PIXELS`] 之内的正常图都放得下，
/// 命中时映射成 [`DecodeCause::OutOfMemory`]。
pub(crate) const MAX_DECODE_ALLOC: u64 = 256 * 1024 * 1024;

/// 解码峰值估算的每像素字节数：RGBA16 中间图（8 B/px，`image` 0.25 能产出的最宽输出类型）
/// + `to_rgba8()` 的 RGBA8（4 B/px）+ ICC 转换的 dst RGBA8（4 B/px，与 `rgba` 同时存活）。
///
/// 12 B/px 是「转换路径」的上界；无 ICC 的资产实际峰值更低，但准入对每张图按统一上界估算
/// （预算宁可保守，不做按 profile 的二次探测——那要在 UI 线程解析 ICC，D-11 明确不这样做）。
const DECODE_PEAK_BYTES_PER_PIXEL: u64 = 12;

/// 解码峰值估算的固定余量：缩放中间图 + `to_rgba8()` + 预乘输出（各在 `MAX_TILE_DIM²`
/// 档位内 ≤ 16 MiB，合取一个 16 MiB 的保守余量）。
const DECODE_PEAK_MARGIN: u64 = 16 * 1024 * 1024;

/// 接受的 ICC profile 字节上限。sRGB / Display P3 / AdobeRGB 的 profile 都是几 KiB；
/// 4 MiB 远高于图片资产的合理需求，只用来拦「塞满 ancillary chunk 的 profile 炸弹」——
/// 超过上限按「无法使用」降级到 sRGB 并计数（不使解码失败）。
pub(crate) const ICC_PROFILE_LIMIT: usize = 4 * 1024 * 1024;

/// 像素的色彩来源分类（D-8；诊断面，`DecodedImage` 内部字段，不公开）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceColor {
    /// 无 ICC，或 ICC 判定为 sRGB：像素**未做任何转换**（与无 profile 资产逐字节一致）。
    Srgb,
    /// 可解析的非 sRGB profile：像素已由 ICC 转换到 sRGB。
    Converted,
    /// 带 ICC 但无法使用（解析失败 / 不支持 / 超大 / 读取失败）：按 sRGB 使用，
    /// 像素 == 无 profile 路径（[`ResourceStats::color_fallback`] 计数）。
    ///
    /// [`ResourceStats::color_fallback`]: crate::resources::ResourceStats::color_fallback
    AssumedSrgb,
}

/// ICC 元数据的读取结果（读取发生在解码像素之前，见模块文档）。
enum SourceIcc {
    /// 解码器没有 ICC profile。
    Absent,
    /// 读到了 profile 字节；能否使用由 [`convert_icc_to_srgb`] 判定。
    Present(Vec<u8>),
    /// 解码器读取 ICC 元数据失败：按「带 profile 但无法解析」降级（不使整图失败）。
    Unreadable,
}

/// 解码结果：**预乘 sRGB 编码**的 BGRA8（行主序，行宽 `width * 4`）。
pub(crate) struct DecodedImage {
    /// 解码（必要时缩放、应用 EXIF orientation）后的像素宽。
    pub width: u32,
    /// 解码（必要时缩放、应用 EXIF orientation）后的像素高。
    pub height: u32,
    /// `width * height * 4` 字节，BGR(A) 顺序。
    pub bgra: Vec<u8>,
    /// 像素的色彩来源（ICC 转换 / sRGB / 降级），`ResourceStats` 的诊断计数来源。
    pub color: SourceColor,
}

/// 字节 → BGRA8 预乘 sRGB（含 ICC → sRGB 色彩管理，D-8）。
///
/// 两段式（R12-15）：先由解码器只读文件头拿尺寸（**不分配整图**），像素 / 单边超限在这里就报
/// [`DecodeCause::TooLarge`]；再 `reserve(total_bytes)` + `set_limits` 后解码，`max_alloc` 命中
/// 映射成 [`DecodeCause::OutOfMemory`]。EXIF orientation 在解码后、缩放前应用到像素；ICC 转换在
/// 缩放 / `to_rgba8` 之后、预乘之前（见模块文档的固定顺序）。
///
/// # Errors
/// [`DecodeCause`]：格式不支持 / 字节损坏 / 源图过大 / 解码器内存不足
/// （四类里除 `Capacity` 外都可能出现）。ICC 无法使用时**不**报错，按 sRGB 降级并计数。
pub(crate) fn decode(bytes: &[u8]) -> Result<DecodedImage, DecodeCause> {
    let format = format_of(bytes)?;
    let tag = format_tag(format);
    // `into_decoder` 让「元数据读取」发生在解码像素之前（orientation / ICC 必须先于 `from_decoder`：
    // 解码器一旦交给 `DynamicImage::from_decoder` 就被消耗，之后再没有 `icc_profile()` 的机会）。
    let mut decoder = ImageReader::with_format(Cursor::new(bytes), format)
        .into_decoder()
        .map_err(|error| classify(tag, error))?;
    let (width, height) = decoder.dimensions();
    check_pixel_dimensions(tag, width, height)?;
    check_source_size(width, height)?;
    // 元数据（D-8）：orientation 与 ICC 都在这里读，位置固定为「reserve/from_decoder 之前」。
    // ICC 读取失败按「带 profile 但无法解析」处理：降级到 sRGB，不让整图解码失败。
    let orientation = decoder
        .orientation()
        .map_err(|error| classify(tag, error))?;
    let icc = match decoder.icc_profile() {
        Ok(Some(profile)) => SourceIcc::Present(profile),
        Ok(None) => SourceIcc::Absent,
        Err(_) => SourceIcc::Unreadable,
    };
    // 复刻 `ImageReader::decode` 的两步（`io/image_reader_type.rs`）：整图分配先记进
    // `max_alloc`，再把剩余预算交给解码器；漏掉任一步都会让 `MAX_DECODE_ALLOC` 失去意义。
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
    if image.width() > MAX_TILE_DIM || image.height() > MAX_TILE_DIM {
        let (target_width, target_height) = fit_within(image.width(), image.height());
        image = image.resize_exact(target_width, target_height, FilterType::Triangle);
    }
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

/// straight-alpha 的 RGBA8（`to_rgba8` 输出）→ sRGB（必要时经 ICC 转换）。
///
/// 返回 `(RGBA8, 色彩来源)`：
/// - `Srgb` / `AssumedSrgb` ⇒ 返回**传入的同一个 `Vec`**（零拷贝：无 ICC / sRGB ICC / 降级路径都
///   不增加一次全图拷贝）；
/// - `Converted` ⇒ 新分配同尺寸 dst 再转换（`try_reserve_exact`，失败 [`DecodeCause::OutOfMemory`]）。
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
    // sRGB profile 必须短路：sRGB→sRGB 的 8-bit 定点 transform 不保证逐字节恒等，转换会破坏
    // 「sRGB 资产与无 profile 资产逐字节一致」的性能 / 黄金护栏。
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
        // Gray / CMYK / DeviceLink 等以 RGBA 为源不支持的 profile：降级 sRGB（像素保持原样）。
        return Ok((rgba, SourceColor::AssumedSrgb));
    };
    let mut converted = allocate_zeroed(rgba.len())?;
    if transform.transform(&rgba, &mut converted).is_err() {
        return Ok((rgba, SourceColor::AssumedSrgb));
    }
    Ok((converted, SourceColor::Converted))
}

/// 判定解析后的 profile 是否**就是 sRGB**（primaries + white point + TRC 数值近似）。
///
/// 同时覆盖 ICC v2 的 `curveType` 采样表与 v4 的 `parametricCurveType`：比较的是 TRC 在采样点上的
/// 数值，而不是内部表示。判定通过后直接跳过转换，因此「sRGB profile 资产逐字节不变」不依赖
/// 转换管线的恒等性。
fn is_srgb_profile(profile: &ColorProfile) -> bool {
    // 只有 matrix-shaper RGB profile 才有可比较的 primaries / TRC；LUT 型（mAB/mft2）与
    // 非 RGB（Gray / CMYK）一律视为「不是 sRGB」，走转换或降级。
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

/// 色度坐标近似相等（容差覆盖 ICC `s15Fixed16` 的量化误差；Display P3 / AdobeRGB 与 sRGB 的
/// 差异在 0.02 以上，1e-3 足以区分）。
fn xyz_close(candidate: &Xyzd, reference: &Xyzd) -> bool {
    const TOLERANCE: f64 = 1e-3;
    (candidate.x - reference.x).abs() < TOLERANCE
        && (candidate.y - reference.y).abs() < TOLERANCE
        && (candidate.z - reference.z).abs() < TOLERANCE
}

/// sRGB TRC 的采样点（覆盖线性段与幂函数段）。
const SRGB_TRC_SAMPLES: [f32; 8] = [0.0, 0.05, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];

/// 三条 TRC 是否与 sRGB 曲线在 [`SRGB_TRC_SAMPLES`] 上数值近似（容差 1e-3 ≈ 0.25/255，
/// 远小于 sRGB 与 gamma 2.2 在低段的差异）。
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

/// 只读文件头拿尺寸（**不解码像素**）：`insert` / `insert_async` 的预算准入用。
///
/// 与 [`decode`] 共用 [`check_source_size`]：超限的图在 UI 线程直接 [`DecodeCause::TooLarge`]，
/// 不会投递到 blocking 池（也就不占解码并发许可）。
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

/// 解码峰值估算（**准入**用，不分配）：文件头尺寸 → 上界字节数。
///
/// `pixels × 12 B`（RGBA16 解码缓冲 8 B + `to_rgba8` 的 RGBA8 4 B + ICC 转换 dst 4 B，
/// 见 [`DECODE_PEAK_BYTES_PER_PIXEL`]）+ [`DECODE_PEAK_MARGIN`]；[`MAX_SOURCE_PIXELS`] 之内的图
/// 估算上界 = 16 M px × 12 B + 16 MiB = 208 MiB ≤ [`MAX_DECODE_ALLOC`]，因此「能合法解码的图」
/// 不会因估算本身被默认预算误拒。
pub(crate) fn estimate_peak(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * DECODE_PEAK_BYTES_PER_PIXEL + DECODE_PEAK_MARGIN
}

/// 解码器的显式限制：`max_alloc` = [`MAX_DECODE_ALLOC`]，单边上限 = [`MAX_SOURCE_DIM`]（strict）。
///
/// 尺寸上限只是纵深防御（精确的 `width` / `height` 由 [`check_source_size`] 报出），
/// 真正要拦住的是「合法单边但总分配爆掉」的输入。
fn decode_limits() -> image::Limits {
    // `image::Limits` 是 `#[non_exhaustive]`，不能用结构体字面量构造。
    let mut limits = image::Limits::no_limits();
    limits.max_image_width = Some(MAX_SOURCE_DIM);
    limits.max_image_height = Some(MAX_SOURCE_DIM);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

/// 源图尺寸判据：像素数上限（主判据）+ 单边上限（纵深防御）。
fn check_source_size(width: u32, height: u32) -> Result<(), DecodeCause> {
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

/// 零尺寸不是可解码的图片（解码器能以 `Ok` 返回 0×0）：分类成 `Corrupt`。
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

/// 输入字节判据（`insert_async` 的轻量校验；UI 线程不做格式嗅探）。
pub(crate) fn check_source_bytes(len: usize) -> Result<(), DecodeCause> {
    if len > MAX_SOURCE_BYTES {
        return Err(DecodeCause::SourceTooLarge {
            bytes: len as u64,
            limit: MAX_SOURCE_BYTES as u64,
        });
    }
    Ok(())
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
        // 走到这里的 `Limits` 是 `max_alloc` 或尺寸上限命中：前者是内存不足，后者是纵深防御
        // （尺寸已由 `check_source_size` 判过，正常路径不该走到，仍如实分类成 `Corrupt`）。
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

/// 等比缩到 `MAX_TILE_DIM` 以内（不放大；两边都至少 1）。
fn fit_within(width: u32, height: u32) -> (u32, u32) {
    let limit = MAX_TILE_DIM as f32;
    let scale = (limit / width as f32).min(limit / height as f32).min(1.0);
    let target_width = ((width as f32 * scale).floor() as u32).clamp(1, MAX_TILE_DIM);
    let target_height = ((height as f32 * scale).floor() as u32).clamp(1, MAX_TILE_DIM);
    (target_width, target_height)
}

/// RGBA8 → 预乘 sRGB 编码的 BGRA8。
///
/// 输出缓冲用 [`Vec::try_reserve_exact`] 分配：分配失败（含容量溢出）映射成
/// [`DecodeCause::OutOfMemory`]，而不是 `vec!` 的 abort。这是本模块自己持有的唯一整图缓冲；
/// `image` 内部的解码 / 缩缓冲仍可能 abort（见模块文档的残余风险说明）。
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

/// `len` 字节的零初始化缓冲；`try_reserve_exact` 失败 ⇒ [`DecodeCause::OutOfMemory`]。
fn allocate_zeroed(len: usize) -> Result<Vec<u8>, DecodeCause> {
    let mut out = Vec::new();
    out.try_reserve_exact(len)
        .map_err(|_| DecodeCause::OutOfMemory {
            limit_bytes: MAX_DECODE_ALLOC,
        })?;
    out.resize(len, 0);
    Ok(out)
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

/// 仅测试：RGBA8 → PNG，可选写入 ICC profile（iCCP）与 EXIF orientation（eXIf）。
///
/// 用 `PngEncoder` 的 `ImageEncoder::set_icc_profile` / `set_exif_metadata` 构造资产，
/// 不依赖手写 iCCP chunk + zlib 压缩。放在模块级是为了让 `resources::tests` 复用同一份
/// 资产构造（ICC 统计口径测试），避免两处重复。
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

/// 仅测试：手工 EXIF：TIFF little-endian，IFD0 里一条 `Orientation` 记录
/// （tag 0x0112 / SHORT / count 1）。
#[cfg(test)]
fn test_exif_orientation(orientation: u16) -> Vec<u8> {
    let mut exif = Vec::with_capacity(26);
    exif.extend_from_slice(b"II*\0");
    exif.extend_from_slice(&8u32.to_le_bytes()); // IFD0 偏移
    exif.extend_from_slice(&1u16.to_le_bytes()); // 条目数
    exif.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    exif.extend_from_slice(&3u16.to_le_bytes()); // SHORT
    exif.extend_from_slice(&1u32.to_le_bytes()); // count
    exif.extend_from_slice(&orientation.to_le_bytes()); // value（后 2 字节是 padding）
    exif.extend_from_slice(&0u16.to_le_bytes());
    exif.extend_from_slice(&0u32.to_le_bytes()); // 下一个 IFD
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
        check_source_bytes, check_source_size, classify, convert_icc_to_srgb, decode,
        decode_limits, estimate_peak, fit_within, is_srgb_profile, premultiply_channel,
        srgb_decode, srgb_encode, test_png, transfer_lut,
    };
    use crate::resources::DecodeCause;

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

    /// 带 iCCP chunk 的 RGBA8 PNG。
    fn png_with_icc(width: u32, height: u32, pixels: &[u8], icc: &[u8]) -> Vec<u8> {
        test_png(width, height, pixels, Some(icc), None)
    }

    /// 带 eXIf chunk 的 RGBA8 PNG（PNG 的 eXIf 是原始 TIFF，不压缩）。
    fn png_with_exif(width: u32, height: u32, pixels: &[u8], orientation: u16) -> Vec<u8> {
        test_png(width, height, pixels, None, Some(orientation))
    }

    /// Display P3 的 ICC 字节（`moxcms` 的默认 profile 编码；与真实 P3 资产同一路径解析）。
    fn display_p3_icc() -> Vec<u8> {
        ColorProfile::new_display_p3()
            .encode()
            .expect("encode display p3 profile")
    }

    /// sRGB 的 ICC 字节。
    fn srgb_icc() -> Vec<u8> {
        ColorProfile::new_srgb()
            .encode()
            .expect("encode srgb profile")
    }

    /// 用 moxcms 直接做一次 RGBA8 转换（不经过解码管线）：测试期望值的独立来源。
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

    /// D-8：EXIF orientation 必须在缩放 / 预乘之前应用（输出宽高与像素顺序都变）。
    #[test]
    fn decode_applies_exif_orientation_before_scaling() {
        // 2×1：左红右蓝（RGBA）。BGRA 后红 = [0,0,255,255]、蓝 = [255,0,0,255]。
        let red_blue = [255u8, 0, 0, 255, 0, 0, 255, 255];

        // EXIF 6 = 顺时针 90°：2×1 → 1×2，上红下蓝。
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

        // EXIF 2 = 水平翻转：尺寸不变，列序反转。
        let flipped = decode(&png_with_exif(2, 1, &red_blue, 2)).expect("fliph png");
        assert_eq!((flipped.width, flipped.height), (2, 1), "EXIF 2 不改尺寸");
        assert_eq!(
            flipped.bgra.as_slice(),
            &[255, 0, 0, 255, 0, 0, 255, 255],
            "EXIF 2 的像素顺序：左蓝右红"
        );
    }

    /// D-8 回归面：无 eXIf 与 orientation=1 必须**逐字节**保持原有解码结果。
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

    /// D-8：Display P3 资产在解码期转换到 sRGB（引擎 RT / 合成假设 sRGB）。
    #[test]
    fn decode_converts_display_p3_to_srgb() {
        let icc = display_p3_icc();
        // 「sRGB 红」在 Display P3 编码下的 8-bit 值（P3 色域更大，同一颜色值更低）。
        let p3_red = [234u8, 51, 35, 255];
        let image = decode(&png_with_icc(1, 1, &p3_red, &icc)).expect("P3 png");

        assert_eq!(image.color, SourceColor::Converted, "P3 必须走转换路径");
        assert_eq!((image.width, image.height), (1, 1));
        // 期望值来自 moxcms 的直接转换（独立于解码管线的同源实现）：转换后回到 sRGB 红。
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

        // 同一份源像素、无 ICC：保持原值（证明转换发生在解码管线，而不是解码器内置行为）。
        let untagged = decode(&png_rgba8(1, 1, &p3_red)).expect("untagged png");
        assert_eq!(untagged.color, SourceColor::Srgb);
        assert_ne!(
            image.bgra, untagged.bgra,
            "带 P3 profile 与无 profile 的像素必须不同"
        );
    }

    /// D-8 性能 / 黄金护栏：sRGB profile 资产必须与无 profile 资产**逐字节**一致。
    ///
    /// 判定为 sRGB 后**短路**，不依赖「sRGB→sRGB transform 恒等」（8-bit 定点管线有量化抖动）。
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

        // 覆盖不透明 / 半透明 / 低段与高段：预乘 LUT 也要逐字节一致。
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

    /// D-8：无 ICC 的资产走现状路径（分类 `Srgb`、逐字节不变、不增加拷贝）。
    #[test]
    fn decode_icc_without_profile_keeps_bytes_exact() {
        let pixels = [10u8, 20, 30, 128, 200, 100, 50, 255];
        let plain = decode(&png_rgba8(2, 1, &pixels)).expect("plain png");
        assert_eq!(plain.color, SourceColor::Srgb);
        // 与 D-7 的 orientation=1 资产逐字节一致（无 ICC 路径不得因 D-8 改动）。
        let tagged = decode(&png_with_exif(2, 1, &pixels, 1)).expect("orientation 1");
        assert_eq!(plain.bgra, tagged.bgra);
    }

    /// D-8：损坏 / 超大 / 不支持（Gray）的 ICC 不得让解码失败，像素按 sRGB 处理并分类计数。
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

    /// D-8 固定顺序：orientation → 缩放 → `to_rgba8` → ICC → 预乘。
    #[test]
    fn decode_applies_orientation_before_icc_and_premultiplies_after() {
        let icc = display_p3_icc();
        // 2×1：左 = P3 编码下的 sRGB 红（不透明），右 = 黑。
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

    /// D-8 性能护栏：无 ICC / sRGB ICC / 降级路径的转换必须**零拷贝**（返回同一个 `Vec`）。
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

    /// R12-15：超过**单边**上限（8193×1 只有 8193 px，像素数远没超，但单边超了）。
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

    /// 判据单测：像素数超限（单边合法）与单边超限都在**分配整图之前**被拦下。
    #[test]
    fn source_size_predicate_covers_pixels_and_single_sides() {
        assert!(check_source_size(1, 1).is_ok());
        assert!(check_source_size(4096, 4096).is_ok(), "16 M px 正好是上限");
        assert!(
            check_source_size(8192, 1).is_ok(),
            "8192×1 = 8192 px：单边合法、像素数远低于上限"
        );

        // 4097×4097 = 16.8 M px > 16 M px：**单边合法但像素超限**（R12-15 的核心场景）。
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

    /// 异步插入的轻量校验：压缩字节 16 MiB。
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

    /// 解码器限制必须**显式**覆盖默认值（默认 512 MiB 会让 8192² RGBA16 以 `Corrupt` 失败）。
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

    /// D-11 / D-8：峰值估算必须覆盖「合法最大图」（含 ICC 转换 dst 的 12 B/px）且不越过
    /// 解码器的 `max_alloc` 上限。
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

    /// D-11：自有 BGRA 缓冲的 `try_reserve_exact` 失败必须映射成 `OutOfMemory`（不 abort）。
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

    /// `Limits` 命中按 `LimitErrorKind` 分类：内存不足走 `OutOfMemory`，其余是 `Corrupt`。
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
