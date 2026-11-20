//! 窗口图标（`WindowOptions::icon`）：PNG / ICO 字节 → `HICON`（`WM_SETICON`）。
//!
//! ## 为什么在平台层解码
//!
//! 图标是**窗口**属性（任务栏 / Alt+Tab），不是应用资源：`WindowOptions::icon` 只在建窗时消费
//! 一次，macOS/Linux 没有等价物（文档化降级，见 [`crate::window::WindowOptions::icon`]）。E6 不
//! 把一个只有 Windows 有的解码路径塞进跨平台资源层（`resources/` 的图集路径语义完全不同：tile、
//! 容量、ICC、预乘），因此解码留在本模块。
//!
//! ## 格式与失败
//!
//! 只接受 **PNG / ICO**（任务栏图标的标准输入；`image::guess_format` 先嗅探，其它格式直接
//! `UnsupportedFormat`）。解码为 RGBA → 转 **BGRA straight alpha**，用 `CreateDIBSection`
//! （32 bpp top-down）+ `CreateBitmap` 的 1 bpp AND mask（alpha=0 的像素置位）构造
//! `ICONINFO`，再 `CreateIconIndirect`：这是 Windows 对 32 bpp alpha 图标的原生表示，任务栏 /
//! 窗口切换器都按 alpha 合成（PNG 压缩的 ICO 帧由 `image` 的解码器展开，不依赖
//! `CreateIconFromResourceEx` 对 PNG 资源格式的版本差异）。
//!
//! 失败一律**强类型**（[`WindowIconCause`] → `EngineError::WindowIcon`）：非法字节让建窗失败，
//! 不静默丢图标（I10）。I/O 与缩放成本只在建窗时付一次。
//!
//! [`WindowIconCause`]: crate::error::WindowIconCause

use std::mem::size_of;

use image::{ImageFormat, load_from_memory_with_format};
use windows::{
    Win32::{
        Foundation::{HANDLE, HWND, LPARAM, WPARAM},
        Graphics::Gdi::{
            BI_BITFIELDS, BITMAPINFO, BITMAPV5HEADER, CreateBitmap, CreateDIBSection,
            DIB_RGB_COLORS, DeleteObject, HBITMAP, HDC, HGDIOBJ,
        },
        UI::WindowsAndMessaging::{
            CreateIconIndirect, DestroyIcon, HICON, ICON_BIG, ICON_SMALL, ICONINFO, SendMessageW,
            WM_SETICON,
        },
    },
    core::BOOL,
};

use crate::{
    error::{EngineError, WindowIconCause},
    window::MAX_WINDOW_ICON_DIM,
};

/// 解码后的图标位图：BGRA8（straight alpha，逐行自上而下，行内无 padding）。
pub(crate) struct IconBitmap {
    /// 像素宽（已校验 ≤ [`MAX_WINDOW_ICON_DIM`]）。
    width: u32,
    /// 像素高（已校验 ≤ [`MAX_WINDOW_ICON_DIM`]）。
    height: u32,
    /// `width * height * 4` 字节的 BGRA。
    bgra: Vec<u8>,
}

/// 解码 `WindowOptions::icon` 的 PNG / ICO 字节。
///
/// # Errors
/// [`WindowIconCause::UnsupportedFormat`]（不是 PNG/ICO）、[`WindowIconCause::DecodeFailed`]
/// （字节损坏）、[`WindowIconCause::TooLarge`]（解码尺寸超过上限）。调用方把它包成
/// `EngineError::WindowIcon { window, cause }`（建窗路径）。
pub(crate) fn decode(bytes: &[u8]) -> Result<IconBitmap, WindowIconCause> {
    let format = image::guess_format(bytes).map_err(|_| WindowIconCause::UnsupportedFormat)?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Ico) {
        return Err(WindowIconCause::UnsupportedFormat);
    }
    let rgba = load_from_memory_with_format(bytes, format)
        .map_err(|_| WindowIconCause::DecodeFailed)?
        .to_rgba8();
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return Err(WindowIconCause::DecodeFailed);
    }
    if width > MAX_WINDOW_ICON_DIM || height > MAX_WINDOW_ICON_DIM {
        return Err(WindowIconCause::TooLarge { width, height });
    }
    let mut bgra = Vec::with_capacity(rgba.as_raw().len());
    for pixel in rgba.pixels() {
        let [red, green, blue, alpha] = pixel.0;
        bgra.extend_from_slice(&[blue, green, red, alpha]);
    }
    Ok(IconBitmap {
        width,
        height,
        bgra,
    })
}

/// 由解码后的位图创建 `HICON`（调用方持有，`NativeWindow::drop` 时 [`destroy`]）。
///
/// # Errors
/// [`EngineError::WindowCreate`]：GDI 对象分配失败（`CreateDIBSection` / `CreateBitmap` /
/// `CreateIconIndirect`）；失败时已分配的 GDI 对象会被回收，不泄漏。
pub(crate) fn create(bitmap: &IconBitmap) -> Result<HICON, EngineError> {
    let width = i32::try_from(bitmap.width).map_err(|_| gdi_error("icon width"))?;
    let height = i32::try_from(bitmap.height).map_err(|_| gdi_error("icon height"))?;
    // top-down DIB（bV5Height 取负）：行序与 `image` 的 RGBA 缓冲一致，不需要翻转。
    let header = BITMAPV5HEADER {
        bV5Size: size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: width,
        bV5Height: -height,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        ..BITMAPV5HEADER::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `header` 是栈上按 BITMAPV5HEADER 布局初始化的输入；`bits` 是输出指针（由系统写入
    // DIB 像素区，长度 = width*height*4）；`hdc` / `hsection` 为 None = 与屏幕兼容、不共享内存。
    let color: HBITMAP = unsafe {
        CreateDIBSection(
            None::<HDC>,
            (&raw const header).cast::<BITMAPINFO>(),
            DIB_RGB_COLORS,
            &mut bits,
            None::<HANDLE>,
            0,
        )
    }
    .map_err(|error| gdi_error(smol_str::format_smolstr!("CreateDIBSection: {error}")))?;
    if bits.is_null() {
        // SAFETY: `color` 是上面成功创建的位图，尚未交给其它所有者。
        unsafe {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        return Err(gdi_error("CreateDIBSection returned a null pixel buffer"));
    }
    // SAFETY: `bits` 指向 DIB 的像素区，长度恰好是 `bgra.len()`（由同一个 width/height 决定）。
    unsafe {
        std::slice::from_raw_parts_mut(bits.cast::<u8>(), bitmap.bgra.len())
            .copy_from_slice(&bitmap.bgra);
    }
    let Some(mask) = create_mask(bitmap) else {
        // SAFETY: 同上（`color` 尚未交给其它所有者）。
        unsafe {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        return Err(gdi_error("CreateBitmap(AND mask)"));
    };
    let info = ICONINFO {
        fIcon: BOOL::from(true),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    // SAFETY: `info` 的位图都是本函数刚创建、尺寸与位深符合 ICONINFO 契约的 GDI 对象；
    // `CreateIconIndirect` 会拷贝位图内容（调用方随后可以删除它们）。
    let icon = unsafe { CreateIconIndirect(&info) };
    // SAFETY: 两个位图由本函数创建、`ICONINFO` 已消费（CreateIconIndirect 拷贝），不再被引用。
    unsafe {
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
    }
    icon.map_err(|error| gdi_error(smol_str::format_smolstr!("CreateIconIndirect: {error}")))
}

/// 1 bpp AND mask：alpha == 0 的像素置位（透明），其余为 0（用颜色位图的 alpha 合成）。
///
/// 32 bpp 图标在现代 Windows 上按 alpha 通道合成，AND mask 是兼容层的兜底；不置位会让完全透明
/// 的像素在旧路径上画成黑色方块。
fn create_mask(bitmap: &IconBitmap) -> Option<HBITMAP> {
    let width = bitmap.width as usize;
    let height = bitmap.height as usize;
    // 1 bpp 的行按 4 字节对齐（DWORD）：`((width + 31) / 32) * 4`。
    let stride = width.div_ceil(32) * 4;
    let mut mask = vec![0u8; stride * height];
    for y in 0..height {
        for x in 0..width {
            let alpha = bitmap.bgra[(y * width + x) * 4 + 3];
            if alpha == 0 {
                // 位图的最高位是行内最左像素。
                mask[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    // SAFETY: `mask` 在调用期间存活；`CreateBitmap` 同步拷贝位数据（1 bpp / 1 plane）。
    let handle = unsafe {
        CreateBitmap(
            bitmap.width as i32,
            bitmap.height as i32,
            1,
            1,
            Some(mask.as_ptr().cast()),
        )
    };
    (!handle.0.is_null()).then_some(handle)
}

/// 把图标应用到窗口的大小 / 小图标槽（`WM_SETICON`）。
pub(crate) fn apply_to_window(hwnd: HWND, icon: HICON) {
    // SAFETY: `hwnd` 由调用方保证存活；`WM_SETICON` 的 lParam 是图标句柄（系统不接管所有权，
    // 由 `NativeWindow` 在销毁窗口后 DestroyIcon）。
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_BIG as usize)),
            Some(LPARAM(icon.0 as isize)),
        );
        SendMessageW(
            hwnd,
            WM_SETICON,
            Some(WPARAM(ICON_SMALL as usize)),
            Some(LPARAM(icon.0 as isize)),
        );
    }
}

/// 销毁由 [`create`] 产出的图标（幂等：`None` / 空句柄直接跳过）。
pub(crate) fn destroy(icon: Option<HICON>) {
    let Some(icon) = icon.filter(|icon| !icon.0.is_null()) else {
        return;
    };
    // SAFETY: `icon` 由 `create` 的 `CreateIconIndirect` 产出、未被系统接管；只销毁一次
    // （`NativeWindow::destroy` / `Drop` 都是幂等路径，字段在销毁后置 None）。
    unsafe {
        let _ = DestroyIcon(icon);
    }
}

/// GDI 失败 → 建窗错误（图标是建窗参数，失败必须让建窗失败，不静默丢图标）。
fn gdi_error(detail: impl Into<smol_str::SmolStr>) -> EngineError {
    EngineError::WindowCreate {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
    use windows::Win32::UI::WindowsAndMessaging::HICON;

    use super::{create, decode, destroy};
    use crate::error::WindowIconCause;

    /// 16×16 的 RGBA 测试图：左上 8×8 不透明红、其余全透明（含不同 alpha 便于校验通道顺序）。
    fn rgba_fixture() -> Vec<u8> {
        let mut rgba = Vec::with_capacity(16 * 16 * 4);
        for y in 0..16u8 {
            for x in 0..16u8 {
                if x < 8 && y < 8 {
                    rgba.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
                } else if x < 12 && y < 12 {
                    rgba.extend_from_slice(&[0x10, 0x20, 0x30, 0x80]);
                } else {
                    rgba.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0x00]);
                }
            }
        }
        rgba
    }

    fn png_bytes(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(rgba, width, height, ExtendedColorType::Rgba8)
            .expect("PNG 编码");
        png
    }

    /// 把一张 PNG 包成单帧 ICO（`image` 的 ICO 解码器同样接受 PNG 压缩帧）。
    fn ico_with_png(png: &[u8], width: u16, height: u16) -> Vec<u8> {
        let mut ico = Vec::with_capacity(22 + png.len());
        ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // type = icon
        ico.extend_from_slice(&1u16.to_le_bytes()); // count
        ico.push(u8::try_from(width % 256).expect("ICO 宽度字节")); // 0 = 256
        ico.push(u8::try_from(height % 256).expect("ICO 高度字节"));
        ico.push(0); // palette
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes()); // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&22u32.to_le_bytes()); // offset
        ico.extend_from_slice(png);
        ico
    }

    /// PNG 解码：尺寸 + BGRA 通道顺序 + straight alpha。
    #[test]
    fn a_png_icon_decodes_to_bgra() {
        let png = png_bytes(16, 16, &rgba_fixture());
        let icon = decode(&png).expect("PNG 图标必须解码成功");
        assert_eq!((icon.width, icon.height), (16, 16));
        assert_eq!(icon.bgra.len(), 16 * 16 * 4);
        assert_eq!(&icon.bgra[0..4], &[0x00, 0x00, 0xFF, 0xFF], "红 → BGRA");
        // (9,0) 是半透明 (0x10,0x20,0x30,0x80)：通道顺序与 alpha 都不变。
        let index = (16 + 9) * 4;
        assert_eq!(
            &icon.bgra[index..index + 4],
            &[0x30, 0x20, 0x10, 0x80],
            "straight alpha 不做预乘"
        );
    }

    /// ICO 解码：PNG 压缩帧同样走 image 的解码器（任务栏图标常见形态）。
    #[test]
    fn an_ico_icon_decodes_the_best_frame() {
        let png = png_bytes(16, 16, &rgba_fixture());
        let ico = ico_with_png(&png, 16, 16);
        let icon = decode(&ico).expect("ICO 图标必须解码成功");
        assert_eq!((icon.width, icon.height), (16, 16));
        assert_eq!(&icon.bgra[0..4], &[0x00, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn unsupported_and_corrupt_payloads_are_strongly_typed_errors() {
        // GIF89a 头：格式可识别但不在 PNG/ICO 白名单。
        assert_eq!(
            decode(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;").err(),
            Some(WindowIconCause::UnsupportedFormat)
        );
        // 完全不是图片。
        assert_eq!(
            decode(b"not an image at all").err(),
            Some(WindowIconCause::UnsupportedFormat)
        );
        // PNG 签名但内容被截断：重新编码后只留前 30 字节。
        let png = png_bytes(16, 16, &rgba_fixture());
        assert_eq!(
            decode(&png[..30]).err(),
            Some(WindowIconCause::DecodeFailed)
        );
    }

    #[test]
    fn oversized_icons_are_rejected_before_any_gdi_work() {
        let width = crate::window::MAX_WINDOW_ICON_DIM + 1;
        let rgba = vec![0u8; (width * 4) as usize];
        let png = png_bytes(width, 1, &rgba);
        assert_eq!(
            decode(&png).err(),
            Some(WindowIconCause::TooLarge { width, height: 1 })
        );
    }

    /// 真实 GDI：`CreateIconIndirect` 产出的 HICON 必须是有效句柄，且可被销毁（不泄漏）。
    #[test]
    fn a_decoded_icon_becomes_a_real_hicon() {
        let png = png_bytes(16, 16, &rgba_fixture());
        let bitmap = decode(&png).expect("解码");
        let icon: HICON = create(&bitmap).expect("CreateIconIndirect 必须成功");
        assert!(!icon.0.is_null());
        destroy(Some(icon));
        destroy(None);
    }
}
