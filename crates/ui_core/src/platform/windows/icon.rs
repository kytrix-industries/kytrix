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

pub(crate) struct IconBitmap {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

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

pub(crate) fn create(bitmap: &IconBitmap) -> Result<HICON, EngineError> {
    let width = i32::try_from(bitmap.width).map_err(|_| gdi_error("icon width"))?;
    let height = i32::try_from(bitmap.height).map_err(|_| gdi_error("icon height"))?;
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
        unsafe {
            let _ = DeleteObject(HGDIOBJ(color.0));
        }
        return Err(gdi_error("CreateDIBSection returned a null pixel buffer"));
    }
    unsafe {
        std::slice::from_raw_parts_mut(bits.cast::<u8>(), bitmap.bgra.len())
            .copy_from_slice(&bitmap.bgra);
    }
    let Some(mask) = create_mask(bitmap) else {
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
    let icon = unsafe { CreateIconIndirect(&info) };
    unsafe {
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
    }
    icon.map_err(|error| gdi_error(smol_str::format_smolstr!("CreateIconIndirect: {error}")))
}

fn create_mask(bitmap: &IconBitmap) -> Option<HBITMAP> {
    let width = bitmap.width as usize;
    let height = bitmap.height as usize;
    let stride = width.div_ceil(32) * 4;
    let mut mask = vec![0u8; stride * height];
    for y in 0..height {
        for x in 0..width {
            let alpha = bitmap.bgra[(y * width + x) * 4 + 3];
            if alpha == 0 {
                mask[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
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

pub(crate) fn apply_to_window(hwnd: HWND, icon: HICON) {
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

pub(crate) fn destroy(icon: Option<HICON>) {
    let Some(icon) = icon.filter(|icon| !icon.0.is_null()) else {
        return;
    };
    unsafe {
        let _ = DestroyIcon(icon);
    }
}

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

    fn ico_with_png(png: &[u8], width: u16, height: u16) -> Vec<u8> {
        let mut ico = Vec::with_capacity(22 + png.len());
        ico.extend_from_slice(&0u16.to_le_bytes());
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.push(u8::try_from(width % 256).expect("ICO 宽度字节"));
        ico.push(u8::try_from(height % 256).expect("ICO 高度字节"));
        ico.push(0);
        ico.push(0);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&22u32.to_le_bytes());
        ico.extend_from_slice(png);
        ico
    }

    #[test]
    fn a_png_icon_decodes_to_bgra() {
        let png = png_bytes(16, 16, &rgba_fixture());
        let icon = decode(&png).expect("PNG 图标必须解码成功");
        assert_eq!((icon.width, icon.height), (16, 16));
        assert_eq!(icon.bgra.len(), 16 * 16 * 4);
        assert_eq!(&icon.bgra[0..4], &[0x00, 0x00, 0xFF, 0xFF], "红 → BGRA");
        let index = (16 + 9) * 4;
        assert_eq!(
            &icon.bgra[index..index + 4],
            &[0x30, 0x20, 0x10, 0x80],
            "straight alpha 不做预乘"
        );
    }

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
        assert_eq!(
            decode(b"GIF89a\x01\x00\x01\x00\x00\x00\x00;").err(),
            Some(WindowIconCause::UnsupportedFormat)
        );
        assert_eq!(
            decode(b"not an image at all").err(),
            Some(WindowIconCause::UnsupportedFormat)
        );
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
